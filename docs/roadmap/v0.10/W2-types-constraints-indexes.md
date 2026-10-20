# W2 — Data Model: Property Types, Constraints, Indexes, and Storage Format

> Part of the v0.10 spec — see [`../v0.10-spec.md`](../v0.10-spec.md). Where this document and the master spec disagree, the master spec wins; its §4 lists every override.

**Workstream:** W2  
**Unit id prefix:** TY-  
**Depends on (other workstreams):** W1 (Cypher functions over new types), W3 (Bolt/PackStream encoding of new types — coordinate canonical representation)

---

## 1. Outcome

After 0.10 a general developer can:

- Store `Date`, `LocalDateTime`, `DateTime` (with UTC offset for lossless roundtrip), `Duration`, and `Binary` (byte blob) properties natively, without encoding them as integers or base64 strings.
- Declare `UNIQUE` constraints on any `(label, property)` pair; the engine refuses a write that would violate them at pre-commit time, across any batch or import path.
- Declare range indexes; `WHERE n.created_at > $t`, `ORDER BY n.score`, and `STARTS WITH` predicates stop scanning full label sets and use a sorted index instead.
- Use standard Cypher DDL — `CREATE INDEX`, `CREATE CONSTRAINT`, `SHOW INDEXES`, `SHOW CONSTRAINTS`, `DROP INDEX`, `DROP CONSTRAINT` — in any query context (HTTP, CLI, Bolt, MCP).
- Upgrade an existing 0.9.x store to 0.10 automatically; a 0.9.x binary that encounters a 0.10 store gets a clear, named refusal rather than silent data loss.

---

## 2. Current State

| Item | Status | Evidence |
|------|--------|----------|
| `Value` enum | `Int(i64)`, `Float(f64)`, `Str(String)`, `Bool(bool)`, `List(Vec<Value>)`, `Map(BTreeMap<String,Value>)` — no date, binary, or temporal types | `crates/core-storage/src/types.rs:5–14` |
| `ValueKey` (index key) | `Int(i64)`, `FloatBits(u64)`, `Str(String)`, `Bool(bool)` — no ordered temporal key | `types.rs:16–22` |
| Column storage | `Column::Int`, `Column::Float`, `Column::Bool`, `Column::Str`, `Column::Mixed` — no typed Date/Binary column | `crates/core-storage/src/columns.rs:143–170` |
| Equality index | `PropertyIndex` backed by `BTreeMap<(label, field), BTreeMap<ValueKey, BTreeSet<u32>>>` — only scalar equality, no range | `crates/core-storage/src/property_index.rs:1–31` |
| Range index | None | `docs/design.md:149` ("R-trees deferred"), `docs/site/indexes.md` |
| Unique constraint | None — only node key is unique | `docs/site/indexes.md`; `crates/core-api/src/db.rs:8239` (`enable_index` only) |
| Cypher DDL | No `CREATE INDEX`, `CREATE CONSTRAINT`, `SHOW`, or `DROP` AST nodes | `crates/core-query/src/cypher/ast.rs:365–488` |
| HTTP DDL surface | No `/schema/indexes` or `/schema/constraints` routes — index creation is Rust API only | `crates/server/src/http.rs:558–604` |
| WAL discriminants | 0–23 in use; 23 is opt-in multiplicity (`SetEdgeCount`); next free is 24 | `crates/core-storage/src/wal.rs:1–186` |
| Snapshot version | V9 (standard) / V10 (multiplicity-enabled); decoder supports V5–V10 | `docs/format-stability.md:22–47` |

**Recon accuracy check:** The recon document (`02-capability-audit.md`) correctly reports no range index, no unique constraint, and no native date/binary types. The equality index is confirmed as `BTreeMap`-based and declared-only (no Cypher DDL). No errors found in those items.

---

## 3. Design

### 3.1 Property value types

#### 3.1.1 New `Value` enum variants

Append to `crates/core-storage/src/types.rs` — bincode discriminants are positional, existing tags 0–5 are frozen:

```
6: Date          — days since Unix epoch (1970-01-01), stored as i32
7: LocalDateTime — microseconds since Unix epoch, no TZ, stored as i64
8: DateTime      — { micros_utc: i64, offset_seconds: i16 }
                    canonical UTC micros + original UTC offset for lossless roundtrip
                    (a "2024-01-01T12:00:00+05:30" roundtrips exactly)
9: Duration      — { months: i32, days: i32, nanos: i64 }
                    follows openCypher Duration: months and days are not normalized against each
                    other (30 days ≠ 1 month); nanos covers sub-day precision
10: Binary       — Vec<u8> — byte blob, no encoding assumption
```

**Lists and Maps:** `List(Vec<Value>)` already wraps any `Value`, so `List([Date(19000), Date(19001)])` is structurally valid and requires no new encoding. Maps spill to `Mixed` column by existing policy — `Map` containing `Date` values is stored the same way. No separate "list of dates" type is needed; the planner treats a list field as unindexable (consistent with current `List` behavior).

#### 3.1.2 Column storage

Add typed column variants to `crates/core-storage/src/columns.rs`:

```
Column::Date   { data: Vec<i32>,  present: Bitmap, adapter: Vec<Value>, live: usize }
Column::LDT    { data: Vec<i64>,  present: Bitmap, adapter: Vec<Value>, live: usize }
Column::DT     { data: Vec<(i64,i16)>, present: Bitmap, adapter: Vec<Value>, live: usize }
Column::Dur    { data: Vec<(i32,i32,i64)>, present: Bitmap, adapter: Vec<Value>, live: usize }
Column::Binary { data: Vec<Vec<u8>>, present: Bitmap, live: usize }
```

`from_first` dispatch: if the first value for a field is `Date`, start `Column::Date`; else if `LocalDateTime`, start `Column::LDT`; etc. Subsequent differing-type values promote to `Column::Mixed` as today.

#### 3.1.3 Ordering / comparison / equality semantics

| Type | Ordering | Cross-type comparison |
|------|----------|----------------------|
| Date | chronological (i32 ascending) | Unordered with DateTime/LDT — no implicit coercion; `type()` distinguishes them |
| LocalDateTime | chronological (i64 ascending) | Unordered with Date/DateTime |
| DateTime | by `micros_utc` ascending; ties broken by `offset_seconds` for stable ordering | Unordered with Date/LDT; `=` compares UTC instant only (two offsets with same UTC are equal) |
| Duration | **not totally ordered** — `1 month` vs `31 days` is indeterminate; only `=` is defined (component-wise); range indexes over Duration are disallowed by the planner | — |
| Binary | lexicographic byte order | Unordered with Str — no implicit coercion |
| Null comparison | null ≠ null (three-valued logic per openCypher) | unchanged |

**Equality for `DateTime`:** Two `DateTime` values are equal iff their `micros_utc` fields are equal. Offset is display-only. This matches GQL semantics.

**Hash for indexes:** `Date → i32.to_le_bytes()`, `LocalDateTime → i64.to_le_bytes()`, `DateTime → micros_utc.to_le_bytes()` (offset not in hash — equal values hash equal), `Binary → sha256 is overkill; use raw bytes as BTreeMap key`.

#### 3.1.4 New `ValueKey` variants

```rust
// append to ValueKey enum (Hash + Ord + Eq already derived):
DateDays(i32),
LocalDateTimeMicros(i64),
DateTimeMicrosUtc(i64),   // offset omitted — equality is on UTC instant
BinaryBytes(Vec<u8>),     // lexicographic; large blobs are slow to index — document the limit
// Duration: not added — range index on Duration is refused
```

`ValueKey::from_value` gains arms for the new variants. `Duration` and `List`/`Map` continue to return `None`.

#### 3.1.5 WAL encoding

`Value` is serialized via bincode inside all WAL records that carry a `Value` field (`SetProp`, `SetPropId`, `InsertNode`, `InsertNodeId`, `Batch`). Because bincode is positional, appending new variants is backward-compatible for writing; a 0.9.x decoder that does not know discriminant 6+ treats the frame as a corrupt tail and stops cleanly — which is the intended fail-safe already documented in `format-stability.md:135-144`. V11 snapshot format guards this: a 0.9.x binary refuses V11 before opening the WAL, so it never encounters unknown Value discriminants in a live store.

#### 3.1.6 Snapshot encoding

The COLUMNS section (id 1) packs column data via `columns.rs` custom pack/unpack. New column kinds need new tag bytes in that binary format (not bincode — see `columns.rs:pack()`). Tag assignment:

| Column kind | Pack tag (u8) | 
|-------------|--------------|
| Int | 0 (existing) |
| Float | 1 (existing) |
| Bool | 2 (existing) |
| Str | 3 (existing) |
| Mixed | 4 (existing) |
| Date | 5 |
| LocalDateTime | 6 |
| DateTime | 7 |
| Duration | 8 |
| Binary | 9 |

Because the COLUMNS section is read with `access_unchecked` after V9 (large-section trust model), adding new column tags is not backward-readable by a V9/V10 decoder — another reason V11 must guard the file.

#### 3.1.7 JSON / HTTP representation

| Type | JSON encoding | Round-trip exact? |
|------|--------------|-------------------|
| Date | `{"$date": "2024-01-15"}` (ISO 8601 date string) | Yes |
| LocalDateTime | `{"$ldt": "2024-01-15T12:34:56.789012"}` (ISO 8601, no TZ) | Yes |
| DateTime | `{"$dt": "2024-01-15T12:34:56.789012+05:30"}` (ISO 8601 with offset) | Yes — offset preserved |
| Duration | `{"$dur": "P1Y2M3DT4H5M6.789S"}` (ISO 8601 duration) | Yes |
| Binary | `{"$binary": "<base64>"}` | Yes |
| Plain Str/Int/Float/Bool | unchanged (bare JSON scalars) | Yes |

This tagged-object convention avoids ambiguity with plain strings. The HTTP query response and ingest body both use it. A bare string `"2024-01-15"` is still `Value::Str`, not `Value::Date` — the caller must use the tagged form for the typed value.

#### 3.1.8 Python binding representation

```python
# mushroomdb Python package — bindings/python/
from datetime import date, datetime, timedelta

# In query results, Value::Date → datetime.date
# Value::LocalDateTime → datetime.datetime (no tzinfo)
# Value::DateTime → datetime.datetime (with tzinfo = timezone.utc + offset)
# Value::Duration → mushroomdb.Duration (named tuple: months, days, nanos) — not timedelta
#   because timedelta cannot represent "1 month" without calendar context
# Value::Binary → bytes
```

`mushroomdb.Duration` is a lightweight named tuple exposed from PyO3. No new third-party Python dependency.

#### 3.1.9 TypeScript client representation

```typescript
// clients/typescript/
type DateValue = { $date: string };           // "2024-01-15"
type LDTValue  = { $ldt:  string };           // "2024-01-15T12:34:56.789012"
type DTValue   = { $dt:   string };           // "2024-01-15T12:34:56.789012+05:30"
type DurValue  = { $dur:  string };           // "P1Y2M3DT4H5M6.789S"
type BinValue  = { $binary: string };         // base64

type PropertyValue = null | boolean | number | string |
                     DateValue | LDTValue | DTValue | DurValue | BinValue;
```

Helpers `asDate()`, `asDateTime()`, `asBinary()` narrow the union type; plain scalars are unchanged.

#### 3.1.10 Arrow representation (arrow-bridge crate)

| mushroomdb type | Arrow DataType |
|-----------------|----------------|
| Date | `Date32` (days since epoch — exact match) |
| LocalDateTime | `Timestamp(Microsecond, None)` |
| DateTime | `Timestamp(Microsecond, Some(offset_string))` |
| Duration | `Duration(Microsecond)` — months/days component **lost**; document this lossiness; a struct array is the lossless alternative but adds complexity |
| Binary | `LargeBinary` |

The Duration Arrow representation is lossy. W1 owns the `duration.inMicroseconds()` Cypher function; the Arrow bridge uses it. Owner decision: accept the lossiness (with documented caveat) or use a struct array for Duration.

#### 3.1.11 Canonical representation for W3 (Bolt/PackStream)

W3 must encode these types in PackStream. The canonical internal representation W3 builds on:

| mushroomdb type | Canonical form | W3 PackStream mapping |
|-----------------|---------------|----------------------|
| Date | `i32` days since Unix epoch | PackStream `Date` (days since epoch — exact) |
| LocalDateTime | `i64` microseconds since epoch | PackStream `LocalDateTime` (nanoseconds — multiply by 1000) |
| DateTime | `(i64 micros_utc, i16 offset_seconds)` | PackStream `DateTimeZoneOffset` |
| Duration | `(i32 months, i32 days, i64 nanos)` | PackStream `Duration` (months, days, seconds, nanos) |
| Binary | `Vec<u8>` | PackStream `Bytes` |

**W3 to confirm:** these mappings, and that it will read/write the exact byte layout specified above. Any drift between PackStream encoding and the internal representation creates correctness bugs in roundtrip through Bolt drivers.

---

### 3.2 Unique constraints

#### 3.2.1 DDL syntax

```cypher
-- Create
CREATE CONSTRAINT constraint_name FOR (n:Label) REQUIRE n.property IS UNIQUE

-- Drop
DROP CONSTRAINT constraint_name

-- Inspect
SHOW CONSTRAINTS
```

`constraint_name` is a required identifier (not optional). Compatible with the openCypher/GQL-adjacent de facto form used by the incumbent server graph DB and the GQL draft. `FOR (n:Label) REQUIRE n.property IS UNIQUE` is the only supported form in 0.10; `IS NOT NULL` is out of scope (see §10).

#### 3.2.2 Internal representation

```rust
// crates/core-storage/src/types.rs (or a new constraints.rs)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniqueConstraint {
    pub name: String,
    pub label: String,
    pub field: String,
}
```

`GraphDb` holds `Vec<UniqueConstraint>` (or `HashMap<String, UniqueConstraint>` keyed by name). At open, constraint declarations are replayed from WAL; the data is not separately snapshot-persisted beyond the META section (bincode, append-only with `#[serde(default)]`).

**META section carries constraints:** Add a `constraints: Vec<UniqueConstraint>` field (serde default empty) to the `V8Meta` struct that populates snapshot section 4. No new snapshot section required. V11 snapshot adds no new section for this — constraints live in META. A V9/V10 store has no `constraints` field; serde fills it as empty — correct behavior.

#### 3.2.3 WAL records (discriminants 24, 25)

```rust
/// Declare a unique constraint. Discriminant 24.
CreateConstraint { name: String, label: String, field: String },

/// Drop a unique constraint by name. Discriminant 25.
DropConstraint { name: String },
```

**Replay behavior:** On WAL replay, `CreateConstraint` registers the constraint declaration without checking existing data. This is identical to how `EnableIndex` and `EnableFulltext` work. The check runs at write time only.

#### 3.2.4 Enforcement point

Check runs inside the write path, **before** the WAL frame is appended (pre-commit). The check is:

```
for each UniqueConstraint on (label, field):
    if the new value V is present in the equality index for (label, field):
        return Err(ConstraintViolation { constraint: name, field, value: V })
```

This requires an equality index on every constrained `(label, field)` pair. When a constraint is created, the engine creates the backing equality index automatically if it does not exist (and marks it as constraint-owned, so `DROP INDEX` cannot drop it without first dropping the constraint). When the constraint is dropped, the index is left in place unless the user also drops it.

**Within-batch uniqueness:** A batch that inserts two nodes with the same constrained value must fail. The pre-commit check runs against a combined view: current index + in-batch inserts. If `(label, field)` sees duplicate values within the batch itself, that is also a violation.

**Rule-derived writes:** Rules fire inside the commit path before WAL append. Constraint checks also run pre-WAL-append. So rule-derived `SetProp` writes from a rule body are checked against constraints. This is the correct behavior and requires no special handling.

**MERGE:** `MERGE` uses the equality index for the match leg; if the constraint is satisfied by the found node, no violation. If `ON CREATE SET` sets a constrained property, the pre-commit check catches a collision.

**CSV import:** Same pre-commit check. Import is batched; if the batch contains duplicates, it is refused with a per-row violation report (row number, key, value).

**As-of reads:** Constraints are metadata; `open_at` does not enforce them (read-only, historical state).

#### 3.2.5 Creating a constraint over existing violating data

Before writing the `CreateConstraint` WAL record:
1. Scan all nodes of the label for the property.
2. If any value appears more than once, collect all (node_key, value) pairs that conflict.
3. Return `Err(ConstraintCreationConflict { violations: Vec<(String, Value)> })` with the full list. The store is unchanged.
4. If no conflicts, write `CreateConstraint` to WAL and enable the equality index (or confirm it exists).

This scan is O(N_label). Budget it as acceptable for the DDL path (infrequent, offline).

#### 3.2.6 Scoped visibility — uniqueness and hidden nodes

The uniqueness check must not leak that a hidden node holds a given value. Design:

The equality index (which backs the constraint check) is a store-global structure — it contains all nodes regardless of scope/role-mask. The uniqueness check therefore runs globally. If a write would violate uniqueness against a hidden node, the error is:

```
constraint violation: property 'email' on label 'User' is declared unique;
a node with that value already exists.
```

No node key, no label variant, no hint that the conflicting node is hidden. The error is identical whether the conflict is with a visible or hidden node. **Trade-off:** a caller probing value space can infer existence of a hidden node by observing which values are rejected. This is documented as an accepted trade-off (owner decision TY-D2 below). The alternative — scope-local uniqueness only — allows duplicate values across scopes, breaking the basic contract of a unique constraint.

---

### 3.3 Range indexes

#### 3.3.1 DDL syntax

```cypher
-- Create (named or anonymous)
CREATE INDEX index_name FOR (n:Label) ON (n.property)
CREATE INDEX FOR (n:Label) ON (n.property)   -- auto-name: idx_Label_property

-- Drop
DROP INDEX index_name

-- Inspect
SHOW INDEXES
```

Composite indexes (ON (n.a, n.b)) are out of scope for 0.10 (see §10).

#### 3.3.2 Structure

A `RangeIndex` is a per-`(label, field)` sorted structure that supports O(log N) range queries. The existing `PropertyIndex` is an equality index (`BTreeMap<ValueKey, BTreeSet<u32>>`). A `BTreeMap` is already inherently ordered, so a range index is the **same data structure** with a different query interface:

```rust
// crates/core-storage/src/range_index.rs  (new file)
pub struct RangeIndex {
    enabled: BTreeSet<(String, String)>,
    forward: BTreeMap<(String, String), BTreeMap<ValueKey, BTreeSet<u32>>>,
    reverse: BTreeMap<(String, String, u32), ValueKey>,
}
```

This is identical in layout to `PropertyIndex`. The two are **unified**: a range index subsumes equality index functionality. Implementation decision (TY-D1): merge `PropertyIndex` and `RangeIndex` into one structure with a kind flag (`Equality` vs `Range`), or keep them separate structs and route planner queries to the correct one based on predicate type.

**Recommendation:** Unify. A range index answers `=`, `<`, `<=`, `>`, `>=`, and range predicates — it is strictly more capable than an equality index. Keep the existing WAL discriminants 21/22 (`EnableIndex`/`DisableIndex`) for the equality kind, and add new discriminants for the range kind. Users who want only equality pay no extra cost — `BTreeMap` lookup is O(log N) for both. This simplifies the planner: both index kinds use the same lookup path.

**W5 note:** Ledger row 102 (key-anchored Cypher lookup 281 ms at 1M nodes) is a planner failure to use an O(1) equality index for a `WHERE key(n) = $k` predicate. That is a planner bug, not an index gap. The range/equality index work here does not subsume it — W5 must fix the planner's key-lookup path separately. Range indexes here help `WHERE n.score > $x`, not `WHERE key(n) = $k`.

#### 3.3.3 WAL records (discriminants 26, 27)

```rust
/// Enable a range index on (label, field). Discriminant 26.
EnableRangeIndex { name: String, label: String, field: String },

/// Disable a range index on (label, field). Discriminant 27.
DisableRangeIndex { name: String },
```

These parallel `EnableIndex` / `DisableIndex`. The name is stored for `SHOW INDEXES` and `DROP INDEX`.

#### 3.3.4 Maintenance: rebuild vs persist in snapshot

Options:
- **Rebuild at open**: same as the existing `PropertyIndex`. O(N_label) scan on open. At 1M nodes with a range index on one field, rebuilding takes roughly the same time as building the full-text index from scratch — estimated 200–400 ms at 1M nodes (no measurement yet). 
- **Persist in snapshot**: like the full-text index (section 13, `FULLTEXT`). Adds a snapshot section; avoids rebuild cost; costs snapshot write time and file size.

**Recommendation:** Rebuild at open for 0.10, with a measured gate: open time with a range index must not exceed 1.5× open time without one at 1M nodes. The full-text index build was already optimized; the range index build (a BTreeMap insert per node) should be cheaper. If the measurement fails, add snapshot persistence as a follow-on (TY-D3 below). Do not add snapshot persistence speculatively — format changes require careful CRC plumbing.

The rebuild-at-open approach means the range index survives snapshots and WAL replay correctly: on any open, declarations are replayed from WAL records, then the index is rebuilt from live data, identical to the existing `PropertyIndex`.

#### 3.3.5 Planner integration

The Cypher planner (`crates/core-query/src/cypher/plan.rs`) already uses `PropertyIndex` for `WHERE n.field = $x`. Extend the planner:

| Predicate shape | Index used | Complexity |
|-----------------|------------|------------|
| `n.field = $x` | equality index or range index (same BTreeMap lookup) | O(log N + matches) |
| `n.field < $x` / `n.field > $x` | range index BTreeMap range query | O(log N + matches) |
| `n.field >= $x AND n.field <= $y` | range index BTreeMap range | O(log N + matches) |
| `n.field STARTS WITH $s` | range index prefix scan (Str columns only) | O(log N + matches) |
| `ORDER BY n.field` with range index | index-order traversal avoids sort | O(N_result) |
| Duration predicates | refused — Duration is unordered | — |

No range index on `Binary` beyond equality (ordering binary blobs for range queries is unusual and of low value in 0.10). Planner uses equality lookup only for Binary.

#### 3.3.6 Interaction with existing indexes

The range index is the superset. An `EnableIndex` (equality, discriminant 21) continues to work for exact-match-only use cases, but the planner treats a range index as also satisfying equality predicates. `SHOW INDEXES` shows both kinds with a `type` column (`equality` / `range`). Composite index interaction: not applicable (composite indexes are out of scope).

---

### 3.4 SHOW INDEXES / SHOW CONSTRAINTS / DROP

#### 3.4.1 Cypher surface

```cypher
SHOW INDEXES
-- returns: name, type (equality/range), label, property, state (online/building)

SHOW CONSTRAINTS
-- returns: name, type (uniqueness), label, property

DROP INDEX index_name
-- error if does not exist; error if constraint-owned (drop constraint first)

DROP CONSTRAINT constraint_name
-- removes constraint; leaves backing equality index in place unless it was
-- constraint-auto-created and has no independent range index
```

All four are new AST statement types in `crates/core-query/src/cypher/ast.rs`.

#### 3.4.2 HTTP routes (new)

```
GET  /schema/indexes      → [{name, type, label, property, state}]
GET  /schema/constraints  → [{name, type, label, property}]
POST /schema/indexes      → body: {name?, label, property, kind: "equality"|"range"} → 201
DELETE /schema/indexes/:name  → 204
POST /schema/constraints  → body: {name?, label, property, type: "unique"} → 201
DELETE /schema/constraints/:name → 204
```

These routes are write-scoped (require full-access token). `POST /schema/indexes` and `POST /schema/constraints` run the same DDL the Cypher statement does.

#### 3.4.3 CLI

```
mushroomdb schema indexes <db-dir>
mushroomdb schema constraints <db-dir>
```

Print tables. `mushroomdb query` with Cypher DDL already covers create/drop.

#### 3.4.4 MCP tools

Extend the MCP `schema` resource (or add `list_indexes` / `list_constraints` tools). DDL via `mushroomdb_query` (Cypher DDL) is sufficient for MCP clients; no separate MCP write tool needed.

---

### 3.5 Storage format version bump

#### 3.5.1 What changes in 0.10

| Change | Format impact |
|--------|--------------|
| New `Value` variants (6–10: Date, LDT, DT, Duration, Binary) | WAL: bincode Value enum gains new discriminants — backward-incompatible for 0.9.x decoders that encounter them; snapshot COLUMNS section gains new column kind tags |
| Constraint declarations in META | Snapshot section 4 (META, bincode, `#[serde(default)]`) gains `constraints` field — backward-compatible for 0.9.x decoders (serde default) |
| Range index declarations in WAL | WAL discriminants 26, 27 — unknown to 0.9.x decoder |
| Unique constraint WAL records | WAL discriminants 24, 25 — unknown to 0.9.x decoder |
| W3 transaction records | **None needed** (master §4.2: a committed transaction is one existing `Batch` frame). Discriminants 28–30 stay unassigned. |

**Snapshot version V11 is opt-in (master §4.1 overrides the rest of this section where they differ).** It is written only by a store that uses a V11 feature — a value of a new type, a unique constraint, or a range-index declaration — and the first V11-only record (including a new value type inside an existing `SetProp` / `InsertNode` / `Batch` record) is preceded by a V11 snapshot in the same call, following `enable_multiplicity()` and V10 exactly (`docs/format-stability.md`, "Discriminant 23 is opt-in"). A store that uses none keeps writing V9 / V10 and stays readable by 0.9.x. Where V11 is written, A 0.9.x binary opening V11 refuses with `snapshot: unsupported version 11` before touching the WAL — identical to the V10 guard documented in `format-stability.md:44`. No silent data loss.

**The V11 container is identical to V9/V10** (same magic, header, section directory). V11 is distinguished by: (a) the `VERSION` field = 11, and (b) the COLUMNS section may carry new column kind tags (5–9). Existing sections are unchanged. No new section IDs are added for 0.10 (constraints go in META via serde default; range index declarations go in WAL replay only, rebuilt at open).

#### 3.5.2 When a store becomes V11

*Replaced by master §4.1.* Opening an old store under 0.10 changes nothing on disk. The store becomes V11 at the first call that would write a V11-only record: that call writes a V11 snapshot first (with the same crash-ordering argument as V10), then appends the record. A WAL-only store gets its first snapshot at that moment. `snapshot::version_for` decides from the store's declared features.

#### 3.5.3 Can 0.9.x open a 0.10 store?

No. A V11 snapshot is refused by name. The WAL is never opened. The refusal message must be:

```
snapshot: unsupported version 11 — this store was written by mushroomdb 0.10 or later.
To read it, upgrade your binary to 0.10. Downgrade is not supported: see docs/format-stability.md.
```

#### 3.5.4 Migration: opt-in, at first use of a V11 feature

No migration at open (master §4.1). The upgrade guide states which features move a store to V11 and that doing so ends downgrade to 0.9.x.

#### 3.5.5 `format-compat` CI

Add a CI job `format-compat` (or extend the existing one):
- Fixture: a V9 snapshot + WAL produced by a pinned 0.9.x binary.
- Test: a 0.10 binary opens it, reads nodes back correctly, writes a new node, snapshots at V11.
- Test: a 0.9.x binary opening a V11 snapshot gets `snapshot: unsupported version 11`, not a panic, not data corruption.
- Test: the golden-fixture pin tests in `crates/core-api/tests/snapshot.rs` get a V11 fixture entry.

---

## 4. Interaction with Differentiators

| Differentiator | Required behavior | Design note |
|----------------|------------------|-------------|
| Rules engine (derived edges) | Constraint checks run against rule-derived `SetProp` writes pre-commit. Range indexes maintained incrementally on rule-fired property writes. | Rule backfill on WAL replay does NOT enforce constraints — replay is identical to the existing rule. |
| Time travel / as-of | `open_at` opens a historical snapshot; constraints and indexes are not enforced on this read-only path. Range index is rebuilt from the historical data state; declarations are replayed from WAL up to the target commit. | Existing `ReadOnly` guard covers writes. |
| Scoped visibility | Uniqueness check is store-global (see §3.2.6 trade-off); range index queries are filtered by the role mask post-lookup, not pre-lookup. The range index contains all nodes; the planner applies the mask after index range resolution. Hidden nodes are not revealed by index results — they are dropped from query output by the mask. | For uniqueness, the trade-off is documented and is an owner decision (TY-D2). |
| Snapshots + WAL replay | Range index is rebuilt at open (not snapshot-persisted). Constraint declarations survive in META (serde default). WAL replay of `CreateConstraint` re-registers but does not re-check. | Replay safety by design — no enforcement on old records. |
| Full-text / vector indexes | No overlap; range index operates on scalar properties. Full-text remains on declared pairs; vector remains rule-driven. SHOW INDEXES returns all three kinds. | — |
| MCP tools | `mushroomdb_query` already exposes Cypher; DDL via Cypher works through it. Schema inspection tool extended. | No new MCP capability required. |

---

## 5. Compatibility & Migration

| Item | Impact | Migration note |
|------|--------|---------------|
| New `Value` variants | **Breaking** for any code that pattern-matches exhaustively on `Value` | Rust: compiler forces a match arm; Python/TS: new tagged-object forms in responses — callers that stringify all values are unaffected |
| New `ValueKey` variants | Internal only | No user-facing break |
| Snapshot V11 | 0.9.x cannot open a V11 store | Clear named refusal; no data loss |
| WAL discriminants 24–30 | 0.9.x truncates WAL at first unknown discriminant | V11 guard prevents 0.9.x from reaching WAL |
| HTTP new routes | Additive | No break to existing clients |
| Cypher DDL new statements | Additive | No break to existing queries |
| `UniqueConstraint` in META | serde default empty | V9/V10 stores read correctly under 0.10 |
| Python `Duration` type | New type returned from queries; existing code getting Duration as `None` or `str` breaks | Document breaking change in changelog |

---

## 6. Verification

### Unit tests (crates/core-storage)

- `Value` bincode roundtrip for each new variant (Date, LDT, DT, Duration, Binary) — verify no tag collision with existing variants 0–5.
- `ValueKey` ordering for Date, LDT, DateTime (UTC equality across offsets).
- `Column::Date` / `Column::LDT` / `Column::DT` / `Column::Binary` pack/unpack identity (custom pack format — not bincode).
- `RangeIndex`: range query returns correct node sets for `<`, `<=`, `>`, `>=`, range, `STARTS WITH`.
- `UniqueConstraint` enforcement: single insert, within-batch duplicate, cross-batch duplicate.
- Constraint creation over violating data: returns `ConstraintCreationConflict` with full list.
- `DROP CONSTRAINT` releases enforcement; value can be duplicated after drop.
- WAL replay of `CreateConstraint` does not enforce against old data (replay-safety).

### Integration / differential probes

- A store written pre-constraint, then opened with constraint declared, does not have old violations enforced during open or WAL replay.
- Behavior identical across: one batch vs separate commits; reopened vs never-closed handle; snapshotting vs never-snapshotting; live vs WAL replay vs `open_at`. Each constraint enforcement scenario tested across all four axes.
- `open_at` of a historical state before a constraint was declared: no constraint visible or enforced.
- `open_at` of a historical state after constraint declared: constraint still not enforced (read-only).
- Rule-derived `SetProp` that violates a unique constraint: write refused pre-commit.
- Scoped query with range index: hidden nodes absent from results, range is not widened to include them.

### Crash sim

- Crash during `CreateConstraint` WAL write (after record appended but before fsync): on reopen, constraint either fully present (replayed) or absent — not half-state.
- Crash during V11 migration (between `.bak` write and new snapshot write): `.bak` is intact; reopen retries migration.

### Performance gates (vs v0.9.x, same hardware)

| Gate | Threshold |
|------|-----------|
| Open time with one range index at 1M nodes | ≤ 1.5× open time without range index |
| Single equality lookup via range index (same BTreeMap path as PropertyIndex) | ≤ 110% of current equality index lookup time |
| Range query (`n.field BETWEEN a AND b`) returning 1000 nodes at 1M total | ≤ 50 ms p50 |
| Unique constraint check overhead per write (one constrained property) | ≤ 5% write latency increase vs unconstrained store |
| Open time of an old V9/V10 store under 0.10 that uses no V11 feature | ≤ v0.9.2 + 5 % (nothing is rewritten) |
| First V11-feature write on a 1M-node store (writes the V11 snapshot first) | ≤ one snapshot write at that size + 10 % |
| First write latency after migration | unchanged |

All comparisons measured before and after on the same hardware. A failing gate blocks the unit.

### Docs checks

- `format-stability.md` updated: V11 wire description, new discriminants 24–30, column kind tags 5–9.
- `docs/site/indexes.md` updated: range index DDL, SHOW INDEXES output, planner note.
- New `docs/site/constraints.md`: unique constraint DDL, enforcement behavior, scoped-visibility trade-off.
- `docs/site/types.md` (new): Date/DateTime/Duration/Binary property types, JSON/Python/TS representations, ordering semantics.
- Changelog: breaking changes — V11 format, new Python `Duration` type, exhaustive `Value` match arms.

### TCK (via W1)

W1 owns the Cypher functions over new types (`date()`, `datetime()`, `duration()`); W2 owns the type storage and DDL. TCK conformance tests for date/time types will exercise W2 storage indirectly. Coordinate with W1 on which TCK categories are blocked on W2 storage being ready.

---

## 7. Build Units

| Unit ID | Description | Size | Files / Crates | Depends on | Conflict hotspots |
|---------|-------------|------|----------------|------------|-------------------|
| TY-1 | New `Value` variants (Date, LDT, DT, Duration, Binary) + `ValueKey` variants — enum, bincode roundtrip, from/to json, unit tests | S | `core-storage/src/types.rs` | — | `types.rs` (shared enum — all W2 units depend on this first) |
| TY-2 | Column storage for new types (`Column::Date`, `LDT`, `DT`, `Dur`, `Binary`) — pack/unpack, from_first dispatch | M | `core-storage/src/columns.rs` | TY-1 | `columns.rs` |
| TY-3 | Python + TypeScript representation — PyO3 arms for new types, `mushroomdb.Duration` namedtuple, TS tagged-union types | S | `bindings/python/`, `clients/typescript/` | TY-1 | — |
| TY-4 | Arrow bridge for new types | S | `crates/arrow-bridge/` | TY-1 | — |
| TY-5 | Unique constraint struct, WAL records 24–25, enforcement pre-commit, constraint-owned index auto-creation | M | `core-storage/src/types.rs`, `core-storage/src/wal.rs`, `core-api/src/db.rs` | TY-1 | `db.rs` (large file — high conflict risk) |
| TY-6 | Constraint creation scan (check existing data before writing `CreateConstraint`) + `ConstraintCreationConflict` error variant | S | `core-api/src/db.rs` | TY-5 | `db.rs` |
| TY-7 | WAL replay safety for `CreateConstraint` (no enforcement on replay); update all WAL replay sites | S | `core-api/src/db.rs` | TY-5 | `db.rs` |
| TY-8 | Unified range index struct (`RangeIndex`), WAL records 26–27, range query API, rebuild-at-open | M | `core-storage/src/range_index.rs` (new), `core-api/src/db.rs` | TY-1 | `db.rs` |
| TY-9 | Cypher DDL parser + AST: `CREATE INDEX`, `CREATE CONSTRAINT`, `SHOW INDEXES`, `SHOW CONSTRAINTS`, `DROP INDEX`, `DROP CONSTRAINT` | M | `core-query/src/cypher/{ast.rs, parser.rs, exec.rs, plan.rs}` | TY-5, TY-8 | `ast.rs`, `exec.rs` |
| TY-10 | Planner: use range index for `<`, `<=`, `>`, `>=`, range predicates, `STARTS WITH`, `ORDER BY` | M | `core-query/src/cypher/plan.rs` | TY-8, TY-9 | `plan.rs` |
| TY-11 | HTTP routes: `/schema/indexes`, `/schema/constraints` (GET + POST + DELETE) | S | `crates/server/src/http.rs` | TY-5, TY-8 | `http.rs` |
| TY-12 | Snapshot V11, opt-in (master §4.1): `version_for` from declared features, V11 snapshot written before the first V11-only record, COLUMNS new pack tags, META `constraints` field (serde default), refusal text on 0.9.x | L | `core-storage/src/snapshot.rs`, `core-storage/src/v8/`, `core-api/src/db.rs`, `docs/format-stability.md` | TY-2, TY-5, TY-8 | `snapshot.rs`, `v8/` — high conflict risk |
| TY-13 | format-compat CI: V9 golden fixture open under 0.10, V11 fixture refused by 0.9.x, pin tests updated | S | `crates/core-api/tests/snapshot.rs`, CI config | TY-12 | `snapshot.rs` tests |
| TY-14 | Performance gates: range index open-time and query-latency benchmarks vs v0.9.x | S | `crates/core-bench/` | TY-8, TY-12 | — |
| TY-15 | Docs: `format-stability.md` V11 section, `indexes.md` range index, new `constraints.md`, new `types.md` | S | `docs/` | TY-9, TY-12 | — |

**Parallelism:** TY-1 must land first (all others depend on it). TY-2, TY-3, TY-4 can run in parallel after TY-1. TY-5 can start after TY-1. TY-6, TY-7 depend on TY-5. TY-8 can run in parallel with TY-5. TY-9 needs TY-5 and TY-8. TY-10 needs TY-9. TY-11 needs TY-5 and TY-8. TY-12 is the last storage unit — needs TY-2, TY-5, TY-8. TY-13, TY-14, TY-15 follow TY-12.

---

## 8. Decisions

### (a) Builder's call

| ID | Decision | Recommendation | Consequence of alternative |
|----|----------|----------------|---------------------------|
| TY-D1 | Merge `PropertyIndex` and `RangeIndex` into one unified struct vs keep separate | **Unify.** A range index is a superset; separate structs duplicate BTreeMap maintenance logic. | Separate structs: two rebuild loops, two planner dispatch paths, risk of divergence. |
| TY-D3 | Range index: rebuild at open vs snapshot-persist | **Rebuild at open.** Simpler; no new snapshot section. Measure open time — if > 1.5× gate, add snapshot persistence as TY-12b. | Snapshot-persist adds a new section, CRC plumbing, migration complexity. Premature if rebuild is fast enough. |
| TY-D4 | Duration Arrow mapping: `Duration(Microsecond)` lossy vs struct array lossless | **Lossy with documented caveat.** Struct array adds complexity and breaks Arrow ecosystem interoperability. Most Arrow consumers care about total duration, not month/day decomposition. | Lossless: breaks Arrow analytics tooling; non-standard. |
| TY-D5 | Auto-name for unnamed `CREATE INDEX FOR ...` | **Auto-name as `idx_<Label>_<property>`.** Forces unique names; prevents accidental duplicate indexes. | No auto-name: user gets an error if they omit the name (worse DX). |
| TY-D6 | Constraint-owned equality index: drop constraint keeps index vs drops it | **Keep index.** Dropping a unique constraint is a schema relaxation; keeping the index is free performance. | Drop index: user must re-create the index if they want equality fast-path. |

### (b) Owner must decide

| ID | Decision | Recommendation | Options |
|----|----------|----------------|---------|
| TY-D2 | Uniqueness check across hidden nodes (scoped visibility) | **Check globally; error does not reveal hidden node.** Weak confidentiality — determined probe can confirm existence. | **A (recommended):** Global check, opaque error — "a node with that value already exists." Consistent uniqueness guarantee; weak confidentiality. **B:** Scope-local check — uniqueness only within the visible scope; allows cross-scope duplicates. Stronger confidentiality but breaks the constraint contract for multi-scope stores. |
| TY-D7 | NOT NULL / existence constraints (`IS NOT NULL`) — include or defer | **Defer to 0.11.** Unique constraints are the higher-value item; NOT NULL adds enforcement complexity at write time with modest payoff. | **A (recommended):** Defer. Ship unique only. **B:** Include — adds `CreateExistenceConstraint` WAL record, one more check in the write path, but minimal design risk since it follows the same pattern as unique. |
| TY-D8 | `Binary` property maximum size | **Owner decides.** Recommend 1 MiB per value (in-memory store, blobs inflate the working set). | **A:** 1 MiB hard limit, enforced at write, reported as `ValueTooLarge`. **B:** No limit — store is bounded by RAM. Large blobs will cause OOM on a 2 GiB store. |

---

## 9. New Dependencies

| Dependency | Crate | Why needed | No-new-dep alternative |
|------------|-------|------------|----------------------|
| `chrono` or `time` | Rust crate | ISO 8601 parsing for Date/DateTime/Duration from JSON/Cypher literals; format to string for HTTP responses | Implement a minimal ISO 8601 parser inline (~200 lines). `chrono` is already a transitive dependency of several crates in the ecosystem; check with `cargo tree` first. |

**Recommendation:** Run `cargo tree | grep chrono` before adding `chrono`. If it is already present transitively, add it directly. If not, implement the minimal parser inline to avoid a new dependency. `time` crate is an alternative; owner may have a preference.

No other new crate dependencies are proposed.

---

## 10. Out of Scope / Deferred

| Item | Reason |
|------|--------|
| Composite indexes `ON (n.a, n.b)` | Significant planner and storage complexity; equality indexes already support `IndexIntersect`; defer to 0.11 |
| NOT NULL / existence constraints | Lower priority than unique; owner decision TY-D7 above |
| Range index snapshot persistence | Do it only if open-time gate fails — see TY-D3 |
| Disk-paged storage / out-of-RAM indexes | Explicitly out of 0.10 per the brief |
| Index on edge properties | Edge properties stored per-edge in section 5 (EDGE_PROPS); adding range index over edge properties requires a new index structure; defer |
| Binary range index (beyond equality) | Lexicographic ordering of arbitrary blobs is rarely useful; defer |
| Duration range index | Duration is not totally ordered; architecturally excluded |
| Multi-property uniqueness constraints | `REQUIRE (n.a, n.b) IS UNIQUE` — more complex enforcement; defer |
| Index build progress reporting | A "building" state for constraint creation over large labels; MVP refuses synchronously instead |
| `RENAME INDEX` / `RENAME CONSTRAINT` | Not in openCypher standard DDL; defer |

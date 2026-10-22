# W1 — Query Language Coverage and Conformance

> Part of the v0.10 spec — see [`../v0.10-spec.md`](../v0.10-spec.md). Where this document and the master spec disagree, the master spec wins; its §4 lists every override.

**Workstream:** W1  
**Unit-id prefix:** QL-  
**Owner:** W1 agent  
**Cross-workstream deps:** W2 (temporal/binary storage representation)

---

## 1. Outcome

After 0.10 a general developer can:

- Run the overwhelming majority of real-world openCypher 9 queries without modification — string, math, list, type-conversion, path and temporal functions all resolve, `REMOVE` works, double-quoted strings are accepted.
- Verify conformance: a published TCK pass rate (target ≥65%) appears in the docs and blocks CI regression.
- Get a precise, actionable error for any feature that remains unsupported: `not supported in mushroomdb 0.10: <feature>`.
- Write `MATCH … REMOVE n.prop` to delete a property, knowing it correctly retracts any derived edge that read that property.
- Use `MERGE` on patterns that include relationships, and `SET n += map` / `SET n = map` map-form assignment.

---

## 2. Current State

### Verified scalar function set
`exec.rs:1687-1706` defines `SCALAR_FUNCS`: `toLower`, `toUpper`, `size`, `coalesce`, `type`, `abs`, `round`, `textMatches` (custom), `contains`, `startsWith`, `endsWith`, `toInteger`, `toFloat`, `toString`, `decay` (custom), `key`, `id`, `labels` — 17 functions total.

### Verified gaps from source

**Lexer** (`lexer.rs:1-61`):
- No `%` (modulo) or `^` (exponentiation) tokens — illegal chars → lex error.
- No double-quoted string: `'"'` falls through to `_ => Err(format!("illegal character ..."))`.
- No `REMOVE`, `FOREACH`, `EXISTS`, `CALL`, `EXPLAIN` keywords in the keyword table (`lexer.rs:239-263`).

**AST / Parser** (`ast.rs`, `parser.rs`):
- `WriteStatement` enum (`ast.rs:368-374`): only `Create`, `MatchSet`, `MatchDelete`, `MatchDeleteNode`, `Merge` — no `Remove` variant.
- `Operand` enum (`ast.rs:200-246`): no `MapLiteral`, no `ListComprehension`, no `PatternComprehension`, no `ExistsSubquery`.
- `UnwindExpr` (`ast.rs:63-70`): only `Lit`, `Prop`, `Var` — no general expression.
- `AggFunc` (`ast.rs:98-107`): `Count`, `Sum`, `Avg`, `Min`, `Max`, `Collect` — no `StDev`, `StDevP`, `PercentileCont`, `PercentileDisc`.
- `BinArith` doc comment (`ast.rs:221-222`): supports `+`, `-`, `*`, `/` only.
- Named path variables (`p = (a)-->(b)`): no `NamedPath` AST node.
- `MergeStmt` (`ast.rs:478-507`): single-node only, no edge patterns, no relationship MERGE.
- `SetClause` (`ast.rs:419-430`): per-property assignment only, no `SET n += map` or `SET n = map`.

**Storage / value layer** (`types.rs:5-14`):
- `Value` enum: `Int`, `Float`, `Str`, `Bool`, `List`, `Map` — no `Date`, `DateTime`, `Duration`, `Binary`.

**WAL** (`wal.rs:28`): `WalRecord::RemoveProp` exists at the storage layer. The Cypher surface is the only missing piece for `REMOVE n.prop`.

**Existing fast paths** (`exec.rs:18-33`, `plan.rs`): `FUSED_SCAN_FIRES`, `SCAN_KEY_FIRES`, `INDEX_SCAN_FIRES`, `INDEX_INTERSECT_FIRES` counters confirm four distinct fast paths. Ledger row 102 (`docs/roadmap/v0.6.10-defects.md`) documents that a key-anchored one-hop read takes 281ms at 1M nodes — the ScanLabel occurs before the key filter. This is a planner bug W1 must fix (QL-02).

**Scoped visibility** (`exec.rs:1319, 2435, 2523-2524`): `view.mask` is threaded through all scan and expand arms; hidden nodes are skipped. Path functions and `id()`/`key()` must obey the same mask — currently they do for node scans but the path length cell (`Cell::Path`) does not carry node ids, so path functions returning constituent nodes would need the mask applied at materialisation time.

**TCK**: No conformance harness exists anywhere in the repository.

---

## 3. Design

### 3.1 User-facing Cypher surface

#### New operators (lexer additions)
```
%   → Tok::Percent   (modulo: a % b)
^   → Tok::Caret     (exponentiation: a ^ b)
"…" → Tok::Str(...)  (double-quoted string, same semantics as single-quoted)
```

#### New keywords
```
REMOVE   → Tok::Remove
FOREACH  → Tok::Foreach   (parse only; emit "not supported in mushroomdb 0.10: FOREACH")
EXISTS   → Tok::Exists    (parse only; emit "not supported in mushroomdb 0.10: EXISTS { }")
CALL     → Tok::Call      (parse `CALL { }` subquery block; emit "not supported in mushroomdb 0.10: CALL subquery")
EXPLAIN  → Tok::Explain
```
`FOREACH`, `EXISTS { }`, `CALL { }` subqueries are parsed to the keyword boundary, then emit the canonical "not supported" error rather than "unexpected token". This prevents apps from getting opaque lex errors on valid openCypher syntax.

#### REMOVE statement
```cypher
MATCH (n:Label) WHERE n.key = $k
REMOVE n.prop [, n.prop2, ...]
```
- Property removal only. Label removal (`REMOVE n:Label`) is not in scope for 0.10 (see §10).
- `REMOVE n.prop` calls `WalRecord::RemoveProp` (already in `core-storage`).
- When a removed property is an input to a rule predicate, the rules engine must retract derived edges exactly as it does for `SetProp`. The existing `core-rules::views::on_prop_change` hook (`core-rules/src/views.rs:385`) already fires on `RemoveProp` — the executor must call `db.remove_prop(key, field)` which routes through that hook.

#### New scalar functions (all added to `eval_func` in `exec.rs`)

**String** (null in → null out; type mismatch → null):
| Function | Signature | Notes |
|---|---|---|
| `trim(s)` | `String → String` | Strip leading/trailing whitespace |
| `ltrim(s)` | `String → String` | Strip leading whitespace |
| `rtrim(s)` | `String → String` | Strip trailing whitespace |
| `replace(s, find, replace)` | `String × String × String → String` | All occurrences |
| `split(s, delim)` | `String × String → List<String>` | Splits on literal delimiter |
| `left(s, n)` | `String × Integer → String` | First n chars; null if n < 0 |
| `right(s, n)` | `String × Integer → String` | Last n chars; null if n < 0 |
| `substring(s, start[, len])` | `String × Integer [× Integer] → String` | 0-based; null if out of range |
| `reverse(s)` | `String → String` | Also handles `List → List` per openCypher |
| `toUpper(s)`, `toLower(s)` | already present | — |

**Math** (null in → null out; non-numeric → null):
| Function | Notes |
|---|---|
| `floor(n)` | Returns `Integer` for integer input, `Float` for float |
| `ceil(n)` | As above |
| `sign(n)` | -1, 0, 1 as `Integer` |
| `sqrt(n)` | Returns `Float`; null if n < 0 |
| `log(n)` | Natural log; null if n ≤ 0 |
| `log10(n)` | Base-10 log; null if n ≤ 0 |
| `exp(n)` | e^n |
| `sin(n)`, `cos(n)`, `tan(n)` | Radians |
| `atan(n)` | Returns Float |
| `atan2(y, x)` | Two-argument arctangent |
| `pi()` | 0-arg; returns `Float(std::f64::consts::PI)` |
| `e()` | 0-arg; returns `Float(std::f64::consts::E)` |
| `rand()` | 0-arg; returns `Float` in [0,1) using `rand::random::<f64>()` |

**List/Map:**
| Function | Signature | Notes |
|---|---|---|
| `head(list)` | `List → Value\|null` | First element; null if empty |
| `tail(list)` | `List → List` | All but first; empty list if singleton |
| `last(list)` | `List → Value\|null` | Last element; null if empty |
| `range(start, end[, step])` | `Integer × Integer [× Integer] → List<Integer>` | Inclusive; step default 1; step=0 → error |
| `keys(map_or_node)` | `Map → List<String>` or `Node → List<String>` | For a node: list of stored property names |
| `properties(node_or_rel)` | `Node\|Rel → Map` | All stored properties as a `Value::Map` |
| `reduce(acc = init, x IN list \| expr)` | Special syntax; see §3.2 | |

**Type-conversion additions:**
| Function | Notes |
|---|---|
| `toBoolean(x)` | String `"true"`/`"false"` → Bool; Int 0/non-0 → Bool; null → null; else null |
| `toBooleanOrNull(x)` | Same but never errors |
| `toIntegerOrNull(x)` | As `toInteger` but null on any failure |
| `toFloatOrNull(x)` | As `toFloat` but null on any failure |

**Node / relationship / path functions:**
| Function | Notes |
|---|---|
| `elementId(n)` | Returns `Str` — same as `id(n)`/`key(n)` (mushroomdb uses string keys) |
| `startNode(r)` | Takes rel variable; returns `Node` cell — caller must not `RETURN` it directly, only via property access; see §3.2 note |
| `endNode(r)` | As above |
| `nodes(path)` | Takes named path variable; returns `List<String>` (node keys in path order) |
| `relationships(path)` | Takes named path variable; returns `List<Map>`, one `{type, start, end}` per relationship in path order (as delivered by QL-07) |
| `length(path)` | Already implicitly supported via `Cell::Path`; expose as explicit function |

**Aggregate additions:**
| Function | Notes |
|---|---|
| `stdev(x)` | Sample standard deviation; null if fewer than 2 values |
| `stdevp(x)` | Population standard deviation |
| `percentile_cont(x, p)` | Continuous interpolation; p ∈ [0,1] |
| `percentile_disc(x, p)` | Discrete (nearest rank) |

#### EXPLAIN
```cypher
EXPLAIN MATCH (n:Label) RETURN n.key
```
Returns a single-column result `plan` containing a JSON string of the plan operators. No execution occurs. Shape:
```json
[{"op":"ScanLabel","label":"Label","var":"n"},{"op":"Project","items":["n.key"]}]
```
Exposed on the existing `POST /query` route by detecting `EXPLAIN` as the first token. No new HTTP route.

#### SET map forms
```cypher
MATCH (n:Label) WHERE key(n) = $k SET n += {score: 1.5, tag: 'x'}   -- merge (keep unlisted props)
MATCH (n:Label) WHERE key(n) = $k SET n = {score: 1.5, tag: 'x'}    -- replace (remove unlisted props)
```
`SET n = map`: issues `RemoveProp` for every existing property not in the map, then `SetProp` for each map entry — all in one WAL batch frame.
`SET n += map`: issues `SetProp` for each map entry only.

#### MERGE with relationships
```cypher
MERGE (a:Person {id: $id})-[:KNOWS]->(b:Person {id: $id2})
ON CREATE SET a.created = true
ON MATCH SET a.seen = true
```
The relationship pattern is matched as a whole; if no such edge exists between the matched/created nodes, the edge is created. Semantics: find-or-create the edge, not the nodes independently. 0.10 supports exactly one relationship in the MERGE pattern; multiple relationships in one MERGE are deferred.

#### Named path variables
```cypher
MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN nodes(p), length(p)
```
`p` binds to a path **map** `{nodes, relationships, length}` held in an ordinary `Cell::Scalar`. The planner emits a `BindPath` op after the pattern's last hop; `nodes(p)` and `relationships(p)` read the map's fields. The mask is applied to each constituent node id when the path is bound (hidden nodes make the path invisible — the whole row is filtered, matching openCypher scoping semantics). a path is a plain `Value::Map` `{nodes: List<String> (node keys, walk order), relationships: List<Map {type, start, end}> (start/end are node keys in the stored direction), length: Int}`, bound as an ordinary `Cell::Scalar` after `PlanOp::BindPath { path_var, start_var, hops: Vec<PathHop { rel_var, to_var, trail_var }> }`. There is no `Cell::NamedPath` (a `Box` variant cost the two-hop aggregate about 9 percent). The mask is checked at `BindPath`, so the row is already dropped when a node is hidden.

#### UNWIND general expression
```cypher
UNWIND range(1, 10) AS i RETURN i
UNWIND [x IN n.tags WHERE x <> 'deleted'] AS tag RETURN tag
```
`UnwindExpr` gains `Expr(Operand)` variant, evaluated at execution time. The executor resolves the operand, checks it is a `Value::List`, and iterates.

#### Error message contract
Every unsupported feature that is syntactically recognised must produce exactly:
```
not supported in mushroomdb 0.10: <feature name>
```
Examples:
- `not supported in mushroomdb 0.10: FOREACH`
- `not supported in mushroomdb 0.10: CALL subquery`
- `not supported in mushroomdb 0.10: EXISTS { } subquery`
- `not supported in mushroomdb 0.10: REMOVE n:Label (label removal)`

Features not yet in the grammar produce the existing `unexpected token` error — that is acceptable for tokens the lexer outright rejects.

### 3.2 Internal design

#### Operand / cell extensions

```rust
// ast.rs additions
pub enum Operand {
    // existing variants …
    MapLit(Vec<(String, Operand)>),       // {a: 1, b: n.x}
    ListComprehension {
        var: String,
        list: Box<Operand>,
        filter: Option<Box<Expr>>,
        map: Box<Operand>,
    },
    Reduce {
        acc_var: String,
        init: Box<Operand>,
        iter_var: String,
        list: Box<Operand>,
        body: Box<Operand>,
    },
}

pub enum WriteStatement {
    // existing variants …
    MatchRemove(MatchRemoveStmt),
    MatchSetMap(MatchSetMapStmt),   // SET n = map / SET n += map
}

pub struct MatchRemoveStmt {
    pub matches: Vec<Pattern>,
    pub where_expr: Option<Expr>,
    /// (var, field) pairs; only property removal in 0.10
    pub removals: Vec<(String, String)>,
    pub returns: Option<Vec<RetItem>>,
}

pub struct MatchSetMapStmt {
    pub matches: Vec<Pattern>,
    pub where_expr: Option<Expr>,
    /// true = merge (+=), false = replace (=)
    pub merge: bool,
    pub var: String,
    pub map: Vec<(String, Operand)>,
    pub returns: Option<Vec<RetItem>>,
}
```

#### Cell extension for named paths (superseded by QL-07: no `Cell` variant was added; the path is a `Cell::Scalar(Value::Map)`. The block below is the original proposal.)

```rust
// exec.rs Cell enum
enum Cell {
    Node(u32),
    Rel(EdgeRef),
    Path(u8),       // legacy: hop count from VarExpand/ShortestPath without path binding
    NamedPath {
        node_ids: Vec<u32>,
        rel_etypes: Vec<u32>,    // etype symbol ids in order
    },
    Scalar(Value),
}
```

#### Plan ops additions
```rust
// plan.rs PlanOp additions
BindPath { path_var: String, start_var: String, hops: Vec<PathHop> },  // as delivered; the first form could not describe a chain
// Emitted after VarExpand/ShortestPath when the pattern is a named path.
// Collects Cell::Node and Cell::Rel from the binding table into Cell::NamedPath.

RemoveProp { var: String, fields: Vec<String> },
// Executor calls db.remove_prop(key, field) for each field.

SetFromMap { var: String, merge: bool },
// merge=true: SET +=; merge=false: SET =
// Executor reads existing props, computes delta, emits RemoveProp + SetProp records.
```

#### Conformance harness

The openCypher TCK is published under the Apache-2.0 licence (confirmed at `github.com/opencypher/openCypher`; the TCK lives in `tck/features/`). Vendoring the Gherkin `.feature` files is safe with attribution.

**Harness location:** `crates/tck-runner/` (new crate, not published to crates.io).

**Fetch strategy:** Vendor the TCK's `.feature` files at a pinned openCypher release tag under `vendor/opencypher-tck/`, with the upstream Apache-2.0 `LICENSE` and a `VERSION` file naming the tag. Not a submodule: agents building in worktrees and CI need no extra checkout step. No runtime download.

**Runner design:**
1. Parse each `.feature` file with a Gherkin parser (`gherkin` crate — see §9).
2. For each scenario: set up an in-memory mushroomdb instance, execute the `Given`/`When`/`Then` steps mapped to Cypher queries, compare results.
3. TCK step mapping covers: `executing query`, `the result should be`, `the result should be empty`, `a SyntaxError should be raised`, `a ConstraintValidationFailed error should be raised`.
4. Scenarios tagged `@ignore` or whose feature files contain tags in the explicit skip list (see below) are marked `EXPECTED_SKIP`.

**Skip list (initial):**
- Features tagged `@CALL_SUBQUERY`, `@FOREACH`, `@GQL`, `@temporalTypes`, `@spatial`, `@schema` — 0.10 out of scope.
- Individual scenarios that require multi-label nodes (mushroomdb is single-label per node).
- Scenarios requiring `PROFILE` output shape.

**Pass-rate computation:**
```
pass_rate = scenarios_passed / (scenarios_total - scenarios_expected_skip)
```
Published on `docs/site/conformance.md` via a CI step that writes the rate into a templated page. The CI gate **fails** if `pass_rate < BASELINE` where `BASELINE` is the rate recorded on the previous successful main-branch run (stored as `tck-baseline.txt` in the repo root, updated by CI on green builds). This means any regression — even one scenario — blocks merge.

**0.10 target pass rate: 65% of non-skipped scenarios.**

Reasoning: the function blitz (QL-03) and syntax fixes (QL-01, QL-04, QL-05) alone move most string/math/list function tests from fail to pass. The areas that remain low (temporal, spatial, CALL, FOREACH) are in the skip list. 65% is achievable with the units in §7 and leaves a realistic margin.

#### key-anchored lookup fix (ledger row 102) — moved to W5 PF-01

**Dropped from W1 (master spec §4.5).** W5 found the precise root cause; PF-01 owns the fix. The paragraph below is kept for context only.


The planner currently emits `ScanLabel` before applying the key filter (`plan.rs` fast-path logic). The fix: when the WHERE clause contains `key(n) = $k` or `id(n) = $k`, the planner must emit `ScanKey { var, key_operand }` as the scan, not `ScanLabel`. The existing `ScanKey` plan op and `SCAN_KEY_FIRES` counter show the infrastructure exists; the planner's identity-eq detection (`plan.rs:818-897`) must be extended to recognise the `key()`/`id()` function form in the WHERE, not just the `{id: …}` inline property form.

### 3.3 Format changes

- **No storage format change.** `Value` enum gains `Date`, `DateTime`, `Duration`, `Binary` variants only if W2 lands them; W1 does not touch the format.
- `WalRecord::RemoveProp` is already in the format (`wal.rs:28`) — no version bump needed.
- The `MatchRemove` and `MatchSetMap` write paths emit only existing WAL records (`RemoveProp`, `SetProp`).

---

## 4. Interaction with Differentiators

### Rules engine
- `REMOVE n.prop`: the executor calls `db.remove_prop(key, field)`. The existing hook (`core-rules/src/views.rs:385`, called after `SetProp`/`RemoveProp`) fires, retrying derivation for any rule that reads `field`. Derived edges whose predicate now evaluates to false are retracted. **Test requirement:** a rule that derives an edge based on `n.score` must retract the edge when `REMOVE n.score` is executed via Cypher.
- `SET n = map` (replace form): the replace form removes props not in the map via `RemoveProp`. The rules hook fires for each removal, as above.
- MERGE with relationships: ON CREATE SET / ON MATCH SET must fire the rules hook for any SET that lands. This is already the case for existing single-node MERGE.
- New scalar functions do not write and do not interact with the rules engine.

### Time travel / as-of
- All new read-only functions (`trim`, `floor`, `keys`, etc.) are evaluated at query time against the `GraphView` provided, which is already the as-of snapshot. No change needed.
- `REMOVE` and `SET` map forms are write operations; they do not interact with as-of reads.
- Named path variables: constituent node ids must be resolved through the same `GraphView`. The mask and as-of view are both on `GraphView`; as long as `NamedPath.node_ids` are resolved through it, time travel is correct.

### Scoped visibility (mask)
- `nodes(path)` and `relationships(path)`: before materialising node keys into the result list, each `node_id` must be checked against `view.mask`. A hidden node makes the containing row invisible — the whole path result is filtered. This matches the behaviour of `expand` (`exec.rs:2523-2524`).
- `elementId(n)`: identical to `key(n)` — the node must be bound (Cell::Node) through a scan that already respects the mask.
- `properties(n)`: reads all stored properties of a bound node. The node is already visible (it came through a masked scan). No additional mask check required.
- `startNode(r)` / `endNode(r)`: the returned node must pass the mask. The executor checks `view.mask.contains(node_id)` before returning; null if hidden.

### Snapshots and WAL replay
- New write statement types (`MatchRemove`, `MatchSetMap`) emit only `RemoveProp` and `SetProp` WAL records. These are existing record types; replay is unaffected.
- EXPLAIN never writes; it runs plan-only and has no WAL interaction.
- The conformance harness uses in-memory instances; no snapshot interaction.
- **Critical:** the planner's key-anchored lookup fix (QL-02) must produce identical results whether the store has been snapshotted or not (the plan uses `topo_view()` / `build_props_view`, both of which work correctly across snapshot boundaries).

### Full-text / vector indexes
- `textMatches()` already exists. No new index interaction.
- `keys()` and `properties()` return stored scalar properties; they do not consult the full-text or vector columns directly. The properties call reads from `build_props_view`; the implementation must not read the vector column (which can be large) — vector properties are excluded from `properties()` output, matching openCypher's convention that `properties()` returns only the property map visible to Cypher.

### MCP tools
- The MCP `query` tool passes Cypher strings to the existing executor. All new functions become available to MCP callers with no MCP-layer change.
- EXPLAIN results are returned as a regular result-set row; the MCP tool returns them as-is.

---

## 5. Compatibility and Migration

- **0.9.x stores open unchanged.** No format version bump.
- **0.9.x reading a 0.10 store:** 0.10 writes no new WAL record types and no new value types (temporal/binary are W2's domain). A 0.9.x binary can open a 0.10-written store without error.
- **API breaks:** `WriteStatement` enum gains two new variants (`MatchRemove`, `MatchSetMap`). Any code matching on `WriteStatement` exhaustively must add arms. This is a Rust compiler error, not a silent break — all such callers are in `core-api/src/db.rs` (the write dispatcher). No public Rust API break because `WriteStatement` is not re-exported from the `mushroomdb` crate's public surface (it is in `core-query`, an internal crate).
- **`UnwindExpr` gains a new variant.** Internal only; same analysis as above.
- **`AggFunc` gains `StDev`, `StDevP`, `PercentileCont`, `PercentileDisc`.** Internal only.
- **`Cell` gains no variant for named paths** (QL-07 delivered the path as a `Value::Map` in a `Cell::Scalar`); no public API break.
- **`SCALAR_FUNCS` constant** (`exec.rs:1687`) must be updated with every new function name so the error message stays accurate.

---

## 6. Verification

### Unit tests (per build unit, in `crates/core-query/src/cypher/tests/`)
- Each new scalar function: null-in→null-out, type-mismatch→null, correct value, 1+ boundary cases.
- `REMOVE n.prop`: property absent after removal; WAL record is `RemoveProp`; rule retraction fires.
- `SET n = map`: unlisted props removed; listed props set; rule hook fires for each removal.
- `SET n += map`: unlisted props unchanged; listed props set.
- Double-quoted strings: identical to single-quoted.
- `%` and `^` operators: correct evaluation, precedence relative to `*`/`/`.
- Named path variables: `nodes(p)` and `length(p)` correct for 1-hop, 3-hop, variable-length.
- `EXPLAIN`: returns plan JSON without side effects.
- MERGE with relationship: edge created when absent; not duplicated when present.
- UNWIND general expression: `UNWIND range(1,5) AS i`, `UNWIND [x IN list WHERE x > 2] AS x`.

### Differential probes
All new write features must pass differential probes across:
- One batch vs separate commits.
- Reopened vs never-closed handle.
- Snapshotting vs never-snapshotting.
- Live vs WAL replay vs `open_at` (read-side features).

### TCK harness (QL-10)
- CI job: `cargo test -p tck-runner` runs the full non-skipped scenario set.
- Gate: fail if pass rate drops below `tck-baseline.txt`.
- On green main, CI updates `tck-baseline.txt` and commits it.
- Docs step: `cargo run -p tck-runner -- --report docs/site/conformance.md` regenerates the conformance page.

### Performance gates (vs v0.9.x benchmark)
| Query shape | Threshold |
|---|---|
| Key-anchored one-hop read at 1M nodes (ledger row 102) | ≤ 10ms (from 281ms — the fix is a planner path, not a scan) |
| String function chain (`toLower` + `trim` + `substring`) at 100K nodes | ≤ 2× baseline |
| `REMOVE n.prop` at 10K nodes | ≤ 2× equivalent `SET n.prop = null` time |

The 1M-node benchmark must be run against v0.9.x to record the baseline, then against 0.10. A reviewer must measure, not reason.

### Docs checks
- `docs/site/query.md` updated to list all new functions with signatures and examples.
- `docs/site/conformance.md` generated by `tck-runner`; must exist and show a rate ≥ 65% before 0.10 ships.
- Every unsupported-feature error message must appear verbatim in the docs ("Unsupported features" section).

---

## 7. Build Units

| ID | Description | Size | Files / crates touched | Depends on | Conflict hotspots |
|---|---|---|---|---|---|
| QL-01 | Lexer: `%`, `^`, double-quoted strings, `REMOVE`/`FOREACH`/`EXISTS`/`CALL`/`EXPLAIN` keywords | S | `core-query/src/cypher/lexer.rs` | — | lexer.rs (shared with all parser work) |
| ~~QL-02~~ | **Dropped** — ledger row 102 is W5 PF-01 (master §4.5) | — | — | — | — |
| QL-03a | Scalar functions: string batch (`trim`, `ltrim`, `rtrim`, `replace`, `split`, `left`, `right`, `substring`, `reverse`) | S | `core-query/src/cypher/exec.rs` | QL-01 | exec.rs SCALAR_FUNCS block |
| QL-03b | Scalar functions: math batch (`floor`, `ceil`, `sign`, `sqrt`, `log`, `log10`, `exp`, `sin`, `cos`, `tan`, `atan`, `atan2`, `pi`, `e`, `rand`) | S | `core-query/src/cypher/exec.rs` | QL-01 | exec.rs SCALAR_FUNCS block |
| QL-03c | Scalar functions: list/map batch (`head`, `tail`, `last`, `range`, `keys`, `properties`) | S | `core-query/src/cypher/exec.rs`, `ast.rs` | QL-01 | exec.rs, ast.rs |
| QL-03d | Scalar functions: type-conversion batch (`toBoolean`, `toBooleanOrNull`, `toIntegerOrNull`, `toFloatOrNull`) | S | `core-query/src/cypher/exec.rs` | QL-01 | exec.rs SCALAR_FUNCS block |
| QL-03e | Scalar functions: node/rel/path functions (`elementId`, `startNode`, `endNode`, `nodes`, `relationships`, `length`) | M | `core-query/src/cypher/exec.rs`, `ast.rs` (the path map from QL-07) | QL-01, QL-07 | exec.rs, ast.rs |
| QL-04 | Aggregate additions: `stdev`, `stdevp`, `percentile_cont`, `percentile_disc` | S | `core-query/src/cypher/{ast,exec}.rs` | QL-01 | ast.rs AggFunc, exec.rs accumulator |
| QL-05 | REMOVE statement: AST, parser, executor, WAL dispatch | M | `core-query/src/cypher/{ast,parser,exec}.rs`, `core-api/src/db.rs` | QL-01 | ast.rs WriteStatement, db.rs write dispatcher |
| QL-06 | SET map forms (`SET n = map`, `SET n += map`) | M | `core-query/src/cypher/{ast,parser,exec}.rs`, `core-api/src/db.rs` | QL-01, QL-05 | same as QL-05 |
| QL-07 | Named path variables: AST `NamedPath`, Cell::NamedPath, `BindPath` plan op | M | `core-query/src/cypher/{ast,parser,plan,exec}.rs` | QL-01 | ast.rs, plan.rs, exec.rs |
| QL-08 | MERGE with relationships | M | `core-query/src/cypher/{ast,parser,exec}.rs`, `core-api/src/db.rs` | QL-01, QL-05 | ast.rs MergeStmt, db.rs |
| QL-09 | UNWIND general expression | S | `core-query/src/cypher/{ast,parser,exec}.rs` | QL-01 | ast.rs UnwindExpr |
| QL-10 | TCK harness: new crate `tck-runner`, vendored `vendor/opencypher-tck/`, CI job, skip list, baseline file, docs page | L | new `crates/tck-runner/`, `vendor/opencypher-tck/` (vendored), `docs/site/conformance.md`, `.github/workflows/` | QL-01 through QL-09 (run after) | CI config, docs |
| QL-11 | EXPLAIN: lexer keyword already in QL-01; plan-to-JSON serialiser, executor short-circuit | S | `core-query/src/cypher/{parser,exec}.rs`, `core-api/src/db.rs` | QL-01 | exec.rs top-level dispatch |
| QL-12 | Unsupported-feature error messages: FOREACH, EXISTS { }, CALL { } parse-and-reject, label REMOVE. **Also** parse `CALL ns.name(args) [YIELD a, b]` (standalone and after `MATCH … WITH`) into a `Call` plan op that dispatches to W3's procedure registry (TX-15) — only `CALL {` is rejected (master §4.5) | M | `core-query/src/cypher/{ast,parser,plan,exec}.rs` | QL-01; TX-15 for end-to-end tests | parser.rs, plan.rs |
| QL-13 | Temporal function surface (date, datetime, duration, time, localtime): syntax parse, evaluation dispatch to W2 value types | L | `core-query/src/cypher/{ast,parser,exec}.rs` | QL-01, **TY-01** (W2 unit providing Date/DateTime/Duration value enum variants and comparison/ordering semantics) | exec.rs, ast.rs |
| QL-14 | `reduce()` function | S | `core-query/src/cypher/{ast,exec}.rs` | QL-01, QL-09 | ast.rs Operand, exec.rs |
| QL-15 | List comprehensions `[x IN list WHERE pred \| expr]` (all three optional-part forms), usable anywhere an expression is — `RETURN`, `WITH`, `WHERE`, `UNWIND`, function arguments. Moved into scope by master §4.5 | M | `core-query/src/cypher/{ast,parser,exec}.rs` | QL-01, QL-09 | ast.rs Operand, exec.rs |

**Parallelism:** QL-01 must complete first (shared lexer). QL-03a/b/c/d, QL-04, QL-09, QL-12 can run in parallel after QL-01. QL-05 and QL-06 can run in parallel after QL-01. QL-07 can run after QL-01. QL-08 needs QL-05 to land first. QL-10 runs last (integration). QL-13 is blocked on W2's TY-01.

---

## 8. Decisions

### (a) Builder's call

**QL-B1: `startNode(r)` / `endNode(r)` return type.**  
Recommend returning `Value::Str` (the node key), not a `Cell::Node`. This avoids embedding raw node handles in the value layer and makes the result directly serialisable. Callers who need to chain `startNode(r).prop` can use a separate MATCH or WITH. Cost: slightly less ergonomic for complex path queries. Alternative (return Cell::Node and support property access on it) requires planner changes across all operators.

**QL-B2: `properties(n)` excludes vector properties.**  
Recommend excluding them (they are large byte arrays, not user-defined scalar props). This matches the behaviour of the incumbent server graph DB. If a user has stored a vector as a named property, it will not appear. This is acceptable for 0.10.

**QL-B3: TCK Gherkin parser — use `gherkin` crate (see §9) or hand-roll.**  
Recommend using the `gherkin` crate. The TCK has hundreds of feature files; hand-rolling a parser for 0.10 is not worth the risk of missing edge cases in the Gherkin syntax. The `gherkin` crate is MIT-licensed.

**QL-B4: MERGE with one relationship only.**  
Recommend limiting to exactly one relationship in the MERGE pattern for 0.10. Multi-relationship MERGE (a chain) requires tracking partial match state across multiple nodes and edges, which is substantially more complex. The vast majority of real-world MERGE usage is single-edge.

### (b) Owner must decide

**QL-O1: TCK git submodule vs vendored copy.** — *Resolved by master §4.6: vendored.*  
Recommended: git submodule pinned to `opencypher/openCypher@2024.2`. Alternative: vendor the feature files as a directory in the repo (no submodule). The submodule keeps the tree lighter but requires `git submodule update --init` in CI. The vendored copy is simpler but adds ~2MB of `.feature` files to the repo. **Consequence of submodule:** CI setup changes; **consequence of vendor:** repo size increase and manual sync to future TCK releases.

**QL-O2: Target TCK pass rate for 0.10.**  
Recommended: 65%. If the scope is reduced (e.g. named paths, MERGE-with-edge, or UNWIND general expr are dropped), the achievable rate falls to approximately 55%. If owner wants 70%+, list comprehensions and pattern comprehensions must be in scope (currently QL out-of-scope). Owner must set the target and the scope accordingly.

**QL-O3: Temporal function scope for 0.10.**  
QL-13 (temporal Cypher surface) is fully blocked on W2 delivering `Date`/`DateTime`/`Duration` value enum variants with comparison/ordering semantics (W2 unit TY-01). If W2 does not deliver TY-01 in time, QL-13 is dropped and temporal function calls emit `not supported in mushroomdb 0.10: temporal functions`. Owner must confirm the W2 dependency timeline and whether partial temporal support (date literals only, no arithmetic) is acceptable.

**QL-O4: `CALL { }` subqueries — parse-and-reject vs not parsed.**  
Recommended: parse the `CALL` keyword and the `{ }` block boundary, emit `not supported` error. Alternative: leave `CALL` as an identifier (current behaviour), which means `CALL db.labels()` produces a confusing parse error rather than a clear message. The downside of the recommended approach is complexity in the parser (must consume the block). Owner decides if the developer-experience improvement justifies the parser complexity.

---

## 9. New Dependencies

| Crate | Version (approx) | Licence | Purpose | No-new-dep alternative |
|---|---|---|---|---|
| `gherkin` | 0.15 | MIT | Parse TCK `.feature` files in `tck-runner` | Hand-roll a minimal Gherkin parser (~500 lines, higher risk) |
| `rand` | 0.8 | MIT/Apache-2.0 | `rand()` function in `eval_func` | Use `std::time` as a seed with a simple LCG — non-uniform, not acceptable for a conformance target |

Both require owner approval per the brief's rule. If `rand` is already in the dependency tree (likely via `core-rules` HNSW), no new dep is introduced. Check with `cargo tree -p core-query | grep rand` before requesting approval.

---

## 10. Out of Scope / Deferred

| Feature | Reason |
|---|---|
| `FOREACH` (execution) | High complexity; specialist use; parse-and-reject covers the error case |
| `EXISTS { }` / `COUNT { }` subqueries | Requires correlated subquery execution model; architectural change for 0.11 |
| `CALL { }` subqueries | Same as EXISTS; parse-and-reject in 0.10 |
| Pattern comprehensions | As above |
| `allShortestPaths` | Moderate complexity; rare; 0.11 |
| Zero-length variable-length paths (`*0..n`) | Parse-time error is documented; 0.11 |
| Spatial types / `point()` | Out of 0.10 brief |
| REMOVE label (`REMOVE n:Label`) | Mushroomdb is single-label; label removal semantics unclear; out of 0.10 |
| `PROFILE` (execution stats) | Requires instrumentation plumbing across the executor; 0.11 |
| `SHOW INDEXES` / `SHOW CONSTRAINTS` / DDL Cypher | Schema DDL surface is a separate workstream decision |
| `LOAD CSV` | Part 1 brief mentions CSV import but as a separate feature (not Cypher LOAD CSV); out of W1 |
| GQL ISO 39075 mandatory features beyond openCypher 9 | Additive; 0.11+ |
| Multi-label nodes | Storage is single-label; not in 0.10 |
| Temporal functions (if W2 TY-01 misses) | QL-13 depends on W2; dropped if dependency not met |

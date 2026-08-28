# Temporal & Memory Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the agent-memory temporal primitives — `edge_history`/`was_linked` with MCP exposure, compare-and-set writes, and history-preserving snapshots — one coordinated WAL-machinery phase.

**Architecture:** Three features share the WAL as their source of truth. `edge_history`/`was_linked` reuse `node_history`'s WAL-scan machinery and horizon contract (Plan 18). CAS preconditions compare against a per-node last-change commit that is maintained incrementally and persisted in a new small V8 section so CAS never goes blind across a snapshot. History-preserving snapshots archive the WAL segment (`wal.<n>.archive`) instead of truncating, extending every history API's horizon backwards under a retention setting.

**Tech Stack:** In-tree Rust only; no new crates. V8 format may gain ONE small section (LAST_CHANGE) — V8 is unreleased, golden_v8 pin regeneration is sanctioned exactly once for it; V5–V7 decode untouched.

**Spec:** `docs/superpowers/specs/2026-08-27-v0.2-association-engine.md` — §Phase C item C1 + Gate GC (history side), §6 items 2 (CAS) and 3 (history-preserving snapshots).

## Global Constraints

- Horizon honesty: every history API states exactly what window it can see; results never silently truncate — the return type carries the horizon (as `node_history` does today).
- Write-side edge validity intervals are OUT of scope (spec non-goal, v0.3 wait-list).
- V8 format: at most one new section (LAST_CHANGE, id 11); `V8_MAGIC_SECTION_COUNT` 12; regenerate golden_v8 once; `mushroomdb verify` picks it up automatically (small section — eager CRC per the T5 ruling, it is <3MiB-class).
- All mutation paths that commit (direct `&mut self`, group-commit queue) must update the last-change map identically; MVCC `ReaderSnapshot` materialization (`reader.rs::apply_one`) must stay coherent with any new WAL records.
- RBAC untouched: role tokens remain read-only; history APIs obey masks the same way `node_history` does (a role must not see history of nodes outside its mask); MCP is trusted-local (outside RBAC) per docs/site/api.md.
- Workspace suite + fmt + clippy --all-targets -D warnings green per task. Conventional lowercase commits, no Co-Authored-By.

---

### Task 1: edge_history + was_linked (core)

**Files:** `crates/core-api/src/db.rs` (new pub APIs beside `node_history`), `crates/core-api/src/history.rs` if `node_history`'s scan helpers live there (follow the existing location), tests `crates/core-api/tests/history.rs` (extend existing file if present, else the file `node_history` tests live in).

**Interfaces:**

```rust
pub struct EdgeHistoryEvent {
    pub edge_type: String,
    pub commit: u64,
    pub event: EdgeEvent,            // Added | Retracted
    pub rule: Option<String>,        // Some(rule) for derived edges, None for manual
}
pub enum EdgeEvent { Added, Retracted }

impl<F: Fs> GraphDb<F> {
    /// All add/retract events between `a` and `b` visible in the current
    /// history horizon (same contract as node_history — the WAL window since
    /// the last truncating snapshot, plus archives when Task 4 lands).
    /// Both directions (a→b and b→a) are reported; direction is recoverable
    /// from the event's edge orientation field if the existing WAL records
    /// carry it — mirror node_history's fidelity, do not invent data.
    pub fn edge_history(&self, a: &str, b: &str) -> Result<HistoryResult<EdgeHistoryEvent>>;
    /// True iff an edge of `edge_type` existed between a and b at `at_commit`.
    /// Errors if at_commit is outside the visible horizon (honesty: never
    /// guess about pre-horizon state).
    pub fn was_linked(&self, a: &str, b: &str, edge_type: &str, at_commit: u64) -> Result<bool>;
}
```

`HistoryResult<T>` = whatever wrapper `node_history` returns today carrying items + horizon metadata — reuse it verbatim; if it is `Vec`-plus-fields, mirror exactly. Events sourced from WAL records: InsertEdge/InsertEdgeId/DeleteEdge (manual, rule: None) and the rule-engine derived add/retract records (rule: Some) — enumerate every WAL record variant that creates or removes an edge and map each; a variant left unmapped is a review finding.

- [ ] **Step 1: failing tests** — manual add → history has Added(rule: None); rule-derived edge fire → Added(rule: Some(name)); prop change causing retraction → Retracted with same rule; delete_edge → Retracted(None); was_linked true between add and retract commits, false after retract, false before add; at_commit below horizon → Err (exact horizon boundary tested both sides); both-direction reporting; masked-role view: node outside mask → history denied/empty consistent with node_history's mask behavior.
- [ ] **Step 2: run to fail. Step 3: implement (reuse node_history scan; single pass collecting both nodes' edge events). Step 4: full suite. Step 5: commit** `feat: edge_history and was_linked temporal reads`

### Task 2: MCP tools + HTTP exposure for history

**Files:** `crates/server/src/mcp.rs` (new tools `edge_history`, and `node_history` if not already a tool — check; add missing only), `crates/server/src/http.rs` (GET endpoints mirroring existing read-endpoint patterns, role-masked), `docs/site/api.md` + `docs/site/timetravel.md` (document horizon contract), `crates/server/tests/http.rs`.

Bindings: HTTP endpoints follow the existing read-handler shape — Full identity via `db.read()`, Role identity masked and coherent (same guard/snapshot discipline as existing read handlers; if `ReaderSnapshot` lacks history scan support, use the `db.read()` guard for BOTH identities and note it — do NOT extend ReaderSnapshot in this task). Role tokens: history of masked-out nodes is absent exactly like the node itself. MCP: trusted-local, no masking, matches existing tool conventions (JSON in/out, error shape).

- [ ] **Step 1: failing tests** — MCP tool returns events for a derived-edge lifecycle; HTTP endpoint role-token cases (visible node history OK, hidden node → same-as-absent); horizon field present in response JSON.
- [ ] **Steps 2-4: fail → implement → full suite. Step 5: commit** `feat: history over mcp and http`

### Task 3: compare-and-set writes

**Files:** `crates/core-api/src/db.rs` (last-change map + CAS API), `crates/core-storage/src/v8/{layout,mod,encode}.rs` + `snapshot.rs` (LAST_CHANGE section 11, count 12), `crates/core-api/src/reader.rs` (apply_one coherence if new WAL records added — prefer NO new WAL record: preconditions are evaluated at commit time, not logged), `crates/server/src/shared.rs` (queue path), `crates/core-api/tests/cas.rs` (new).

**Interfaces:**

```rust
pub enum Precondition {
    /// Node's last-change commit must equal `expected` (u64 commit seq).
    NodeUnchangedSince { key: String, expected: u64 },
    /// Node must not exist.
    NodeAbsent { key: String },
}
#[derive(Debug)]
pub struct CasConflict { pub key: String, pub expected: u64, pub actual: u64 } // typed error variant on GraphError
impl<F: Fs> GraphDb<F> {
    /// Returns the last-change commit for a node (u64), or None if unknown key.
    pub fn last_changed(&self, key: &str) -> Option<u64>;
    /// write_batch with preconditions checked atomically before any op applies.
    /// ALL preconditions pass or the whole batch rejects with GraphError::CasConflict.
    pub fn write_batch_cas(&mut self, preconds: Vec<Precondition>, ops: /* same shape as write_batch */) -> Result<(usize, usize)>;
}
// SharedDb::submit_batch_cas(preconds, ops) — queue path: preconditions evaluated
// by the drain thread under the write guard immediately before the batch applies
// (no TOCTOU between check and apply — same guard).
```

Bindings: last-change map = `HashMap<u32 /*node id*/, u64 /*commit*/>` updated on every committed mutation touching the node (insert/prop/label/edge-endpoint changes count as touching both endpoints — document the exact "touch" definition in the doc-comment and TEST it); persisted as V8 section 11 (bincode, small — 8-16 bytes/node); loaded eagerly at open (it is small-section class); WAL replay rebuilds increments; legacy V5-V7 stores open with the map seeded from the WAL window only, and `last_changed` for pre-horizon nodes returns the snapshot-boundary commit (document: CAS against legacy stores is horizon-bounded until first V8 snapshot). No new WAL record types.

- [ ] **Step 1: failing tests** — CAS success when expected matches; CasConflict with correct actual on mismatch; NodeAbsent conflict when node exists; batch atomicity (multi-op batch with one failing precond applies NOTHING, WAL gains no frame); queue path via submit_batch_cas under concurrent writers (loser of a race gets CasConflict, winner commits — deterministic with a barrier); map survives snapshot+reopen (V8 section round-trip); edge insert touches both endpoints; golden_v8 regenerated once and pinned.
- [ ] **Steps 2-4: fail → implement → full suite. Step 5: commit** `feat: compare-and-set writes with persisted last-change commits`

### Task 4: history-preserving snapshots

**Files:** `crates/core-api/src/db.rs` (snapshot archive rotation + retention), `crates/core-storage/src/fs.rs` (archive file ids), history scan (Task 1 machinery) + `node_history` + `open_at` gain archive-scanning, `crates/cli/src/lib.rs` (retention flag on snapshot cmd + serve), tests in `crates/core-api/tests/` + crash-window coverage in the sim-harness sweep.

**Interfaces:**

```rust
pub struct SnapshotOptions { /* existing fields */ pub archive_wal: bool /* default false */ }
/// Retention: keep the newest N archives (config on GraphDb, default e.g. 8; 0 = unlimited OFF, feature opt-in only).
pub fn set_wal_archive_retention(&mut self, keep: Option<u32>);
```

Bindings: on `snapshot(archive_wal: true)`, the WAL segment is renamed to `wal.<snapshot_commit>.archive` (rename, not copy — atomic on same fs) BEFORE the fresh WAL starts; crash windows: a crash between archive-rename and new-WAL creation must reopen correctly (archived segment is NOT replayed into live state — it is pre-snapshot by construction; enumerate and test the windows). History APIs (`node_history`, `edge_history`, `was_linked`, `open_at`) accept the extended horizon: scan archives newest-first, chaining commit ranges; horizon metadata reports the true reachable floor. Retention prunes oldest archives at snapshot time (deletion is the ONLY deletion; document). Default behavior unchanged (archive_wal: false → truncate as today) — byte-identical for existing users.

- [ ] **Step 1: failing tests** — archive created with correct name/commit range; history spans two snapshot boundaries with archives on; open_at reaches a pre-snapshot commit through an archive; retention prunes to N keeping newest; default path byte-identical (existing snapshot tests untouched); crash between rename and new-WAL reopens clean (sim-fs sweep case); archives excluded from live replay.
- [ ] **Steps 2-4: fail → implement → full suite. Step 5: commit** `feat: history-preserving snapshots with wal archives`

### Task 5: docs + phase gate

**Files:** `docs/site/timetravel.md` (rewrite: full temporal story — history APIs, CAS, archives, retention, horizon contract), `docs/site/api.md`, `README.md` (temporal section — this is the Zep-differentiator, lead with it honestly), `llms.txt`.

- [ ] Gate check (controller-assisted): Gate GC history side — edge_history/was_linked tested incl. horizon + retraction; MCP tools live; CAS + archives documented with dates. Commit `docs: temporal and memory story`.

---

## Gate (phase)

- All task tests green; workspace green; fmt+clippy clean.
- Default-path byte-identical for users not opting into archives; no format break beyond the sanctioned section 11.
- Gate GC (history portion) satisfied; C2/C3 reach items are the PARALLEL track, not this plan.

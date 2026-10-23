use crate::durable_wal::{DurableWal, WalPosition};
use crate::ingest::{IngestOptions, IngestReport};
use crate::roles::{PropPredicate, RoleDef, RolesFile, WriteScope};
use crate::subscription::{
    event_matches, DbEvent, SubEntry, SubFilter, SubInner, Subscription, DEFAULT_SUB_CAPACITY,
};
use core_query::cypher::ast::{ret_val_label, ArithOp};
use core_query::cypher::{
    execute, execute_union, is_subscribable, is_write_tokens, lex, parse, parse_read, parse_write,
    plan, Expr, MatchDeleteNodeStmt, NodePat, Operand, Params, Pattern, PlanOp, Query, RetItem,
    RetVal, WriteStatement,
};
use core_query::{
    eval_filter, expand, neighborhood, values_equal, Dir, Filter, GraphView, ResultSet,
};
use core_rules::{
    decode_rule_def, ef_max, evaluate, BuildProgress, EngineEdgeDelta, GraphMut, NodeView,
    Predicate, RuleDef, RuleEngine, ViewDef, ViewStore,
};
use core_storage::fs::{FileId, Fs, FsIntrospect, RealFs};
use core_storage::fulltext::FulltextIndex;
use core_storage::property_index::PropertyIndex;
use core_storage::v8::encode::{
    archived_hnsw_to_owned, archived_rules_meta_to_owned, archived_to_idmap, archived_to_interner,
    archived_views_to_owned, decode_last_change_bytes, decode_meta, encode_v8, V8Meta,
};
use core_storage::v8::seam::TopologyView;
use core_storage::wal::{decode_all, encode_record, try_encode_record, WalRecord};
use core_storage::EdgePropsView;
use core_storage::{
    namespace_of_value, ColumnStore, Direction, EdgeProps, GraphError, IdMap, Interner, Result,
    Topology, UniqueConstraint, Value, ValueKey, BINARY_MAX_BYTES,
};
pub use core_storage::{valid_namespace, NS_DEFAULT, NS_MAX_LEN, NS_PROP};

/// Index of [`NS_DEFAULT`] in `GraphDb::ns_names` — always zero, so the
/// open-time pass over a store with no `ns` column fills `node_ns` with one
/// constant and allocates no names.
const NS_DEFAULT_IDX: u32 = 0;

mod call;
mod ddl;
pub use ddl::{SchemaError, SchemaErrorKind};
mod row_eval;
mod unwind_write;

pub(crate) use call::{call_statement, run_call};
pub(crate) use ddl::{
    ddl_statement, schema_change_in_a_transaction, schema_change_on_a_read, SchemaListing,
};

/// The reserved edge property holding a pair's insert count (§5.13).
///
/// Absent means 1 — the count is written only from the second insert of a
/// triple onward, and only on a store that called
/// [`GraphDb::enable_multiplicity`]. The engine owns the name: Cypher `SET` on
/// it is refused, as the other reserved names are.
pub const EDGE_COUNT_PROP: &str = "count";

/// The conflict-set entry of a key that changed between a transaction's BEGIN
/// and the moment the transaction first touched it. No node's `last_change`
/// is ever `u64::MAX`, so a `NodeUnchangedSince` carrying it always fails —
/// the same sentinel `Precondition::NodeAbsent` reports as `expected`.
pub(crate) const TX_CHANGED_SINCE_BEGIN: u64 = u64::MAX;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// Print a timing checkpoint when MUSHROOMDB_TRACE_OPEN is set.
/// Zero-cost when the env var is absent (the var check is O(1) after first call).
macro_rules! trace_open {
    ($phase:literal, $t:expr) => {
        if std::env::var("MUSHROOMDB_TRACE_OPEN").is_ok() {
            eprintln!(
                "[MUSHROOMDB_TRACE_OPEN] {:40} {:>9.3?}",
                $phase,
                $t.elapsed()
            );
        }
    };
}

/// Print a migration phase checkpoint when MUSHROOMDB_TRACE_MIGRATE is set.
/// Zero-cost when the env var is absent (the var check is O(1) after first call).
macro_rules! trace_migrate {
    ($phase:literal, $t:expr) => {
        if std::env::var("MUSHROOMDB_TRACE_MIGRATE").is_ok() {
            eprintln!(
                "[MUSHROOMDB_TRACE_MIGRATE] {:40} {:>9.3?}",
                $phase,
                $t.elapsed()
            );
        }
    };
}

// Test-only: counts how many times `pending_deltas_since().to_vec()` actually
// executes (i.e., at least one view is defined). Used to verify the fast-path
// guard skips the allocation when `view_store.is_empty()`.
#[cfg(test)]
thread_local! {
    static DELTA_COPY_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// Per-thread count of query-subscription `execute` calls in `distribute_events`.
//
// Incremented each time a query subscription actually runs its plan (i.e.,
// the label-skip fast-path did not fire). Because `distribute_events` is
// called synchronously on the writer thread, this thread-local correctly
// isolates each test thread's count even when integration tests run in
// parallel. Read via [`query_sub_exec_count`].
thread_local! {
    static QUERY_SUB_EXECS_TL: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Return the number of query-subscription re-executions logged on this
/// thread since the process started (or since last reset via
/// [`reset_query_sub_exec_count`]).
///
/// Primarily for integration tests that verify the label-skip fast-path.
#[doc(hidden)]
pub fn query_sub_exec_count() -> usize {
    QUERY_SUB_EXECS_TL.with(|c| c.get())
}

/// Reset the per-thread query-subscription execution counter to zero.
#[doc(hidden)]
pub fn reset_query_sub_exec_count() {
    QUERY_SUB_EXECS_TL.with(|c| c.set(0));
}

// Exact-versus-approximate warnings emitted on this thread. Thread-local for
// the same reason [`QUERY_SUB_EXECS_TL`] is: integration tests run in parallel
// and each gets its own thread, so a neighbour's masked search cannot be
// mistaken for this test's.
thread_local! {
    static AMBIGUOUS_EXACTNESS_WARNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static AMBIGUOUS_EXACTNESS_LAST: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// How many times a masked, non-exact vector search has explained itself on
/// this thread since the last [`ambiguous_exactness_warns_reset`].
///
/// The line itself is the product; this counter exists so a test can assert it
/// is printed **once per index** rather than once per call.
///
/// **Single-threaded assertions only.** The suppression set this counts is a
/// `Mutex<HashSet<_>>` on the `GraphDb` — shared by every thread — while the
/// counter is thread-local. Under a concurrent caller (`serve`, which is the
/// deployment the warning exists for) the thread that prints the line is not
/// necessarily the thread that asked, so a zero here does not mean the line was
/// not printed and a one does not mean it was printed once. It answers
/// "once per index" only in a test that owns the store.
#[doc(hidden)]
pub fn ambiguous_exactness_warns() -> u64 {
    AMBIGUOUS_EXACTNESS_WARNS.with(|c| c.get())
}

/// The most recent exactness warning printed on this thread, verbatim.
///
/// The advice a caller reads has to be advice that caller can act on, which the
/// counter alone cannot witness — see
/// `the_hybrid_path_advises_a_call_the_hybrid_caller_can_make`. Carries the
/// same single-threaded caveat as [`ambiguous_exactness_warns`].
#[doc(hidden)]
pub fn ambiguous_exactness_last_warning() -> Option<String> {
    AMBIGUOUS_EXACTNESS_LAST.with(|c| c.borrow().clone())
}

/// Reset this thread's exactness-warning counter and recorded line.
#[doc(hidden)]
pub fn ambiguous_exactness_warns_reset() {
    AMBIGUOUS_EXACTNESS_WARNS.with(|c| c.set(0));
    AMBIGUOUS_EXACTNESS_LAST.with(|c| *c.borrow_mut() = None);
}

/// Which signature reached the approximate masked vector leg.
///
/// One code path, two entry points, and the advice cannot be the same: a
/// warning that names an argument the caller's function does not take sends
/// them looking for a parameter that is not there. `search_hybrid` and
/// `search_hybrid_scoped` take `(text_field, query_text, vector_field,
/// query_vec, label, k[, mask])` — no `exact`, no `where`.
///
/// The warning is not suppressed on the hybrid path. The approximation is the
/// same one, and a caller who read `mask=` as a promise of exhaustiveness is
/// the reader it was written for whichever door they came in by; only the
/// remedy differs, so only the remedy changes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum ExactnessCaller {
    /// `find_similar` / `find_similar_vector_*` — `exact` and `where` are its
    /// own parameters.
    Vector,
    /// `search_hybrid` / `search_hybrid_scoped` — neither argument exists, and
    /// the leg is one half of a fusion.
    Hybrid,
}

impl ExactnessCaller {
    fn subject(self) -> &'static str {
        match self {
            Self::Vector => "a masked vector search",
            Self::Hybrid => "the vector leg of a masked hybrid search",
        }
    }

    fn advice(self) -> &'static str {
        match self {
            Self::Vector => "pass exact=True or a where= predicate.",
            // Names the call that does take the argument, because this one
            // does not: the caller's own next step, not a parameter hunt.
            Self::Hybrid => {
                "run the vector leg on its own with find_similar(field, vector, mask=…, \
                 exact=True) and fuse it with search() yourself — search_hybrid itself \
                 takes no exactness argument."
            }
        }
    }
}

/// Internal state for a single `subscribe_query` subscription.
///
/// On every commit, `distribute_events` re-executes `ops` against the current
/// graph state, diffs the result against `prev_rows`, and pushes
/// `DbEvent::QueryRowAdded` / `QueryRowRemoved` events to `inner`.
///
/// **Full re-run per commit; use LIMIT to bound execution cost.**
/// (Differential evaluation is roadmap / Phase 5.)
pub(crate) struct QuerySubEntry {
    /// Compiled plan for the subscribed Cypher query.
    ops: Vec<PlanOp>,
    /// Column names from the first execution (fixed for the subscription lifetime).
    columns: Vec<String>,
    /// Serialized (JSON) row key → row data, representing the result set at
    /// the end of the last commit. Used to diff against the new result.
    prev_row_map: std::collections::HashMap<String, Vec<Option<Value>>>,
    /// Weak pointer to the subscriber queue; dead Weak → subscription dropped.
    inner: std::sync::Weak<SubInner>,
    /// Interned label sym captured at subscribe time from the plan's leading scan
    /// (`ScanLabel`, `IndexScan`, or `IndexIntersect` with a concrete label).
    ///
    /// `None` means the plan has an `Expand` op (or no recognizable leading scan
    /// with a concrete label), and this subscription must re-execute on every
    /// commit without skipping. This is the conservative v0.4.3 boundary: Expand
    /// queries are never skipped because edges can alter join results regardless
    /// of which node labels were written.
    scan_label: Option<u32>,
}

/// A post-commit mutation notification.
///
/// Emitted from `log_then_apply` after the WAL append, fsync, and
/// in-memory `apply` all succeed. Never emitted for rejected operations
/// (validation errors, [`GraphError::RuleOwned`], duplicate keys, no-op
/// deletes/removes). Event payloads carry user keys and rule names, never
/// internal ids.
///
/// **Replay:** [`GraphDb::open`] / [`GraphDb::open_with`] replay the WAL via
/// `apply` only. Emission lives exclusively in `log_then_apply`, so
/// recovery is silent even if a sink were installed (it cannot be: the
/// sink is in-memory and set after open).
///
/// **Ordering:** a `Batch` WAL frame emits one event per inner record, then
/// [`MutationEvent::BatchApplied`]. An ingest commit emits those same inner
/// events, then [`MutationEvent::Ingested`] (not `BatchApplied`). An empty
/// or all-noop batch writes no WAL and emits nothing (including no summary).
///
/// **Derived edges:** rule-created or retracted edges are not individually
/// evented — they are recoverable from the triggering mutation plus the live
/// rule set. Only the triggering record is emitted.
///
/// **Wire form:** externally tagged snake_case JSON
/// (`{"node_inserted":{"label":"A","key":"k"}}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationEvent {
    NodeInserted {
        label: String,
        key: String,
    },
    PropSet {
        key: String,
        field: String,
    },
    PropRemoved {
        key: String,
        field: String,
    },
    EdgeInserted {
        edge_type: String,
        src: String,
        dst: String,
    },
    EdgeDeleted {
        edge_type: String,
        src: String,
        dst: String,
    },
    NodeDeleted {
        key: String,
    },
    RuleCreated {
        name: String,
    },
    RuleDeleted {
        name: String,
    },
    RuleRebuilt {
        name: String,
    },
    BatchApplied {
        ops: usize,
    },
    Ingested {
        label: String,
        inserted: usize,
    },
}

/// Whether `rec` is the frame [`GraphDb::create_rule`] logs: a `CreateRule`,
/// alone or behind the `Intern` record for its edge type.
///
/// `create_rule` goes through the dense rewrite like every other write, so its
/// frame is `Batch([Intern { edge_type }, CreateRule])` rather than a bare
/// `CreateRule`. Anything that asks "was this commit a rule creation" must
/// accept both shapes, or it silently stops recognising the one the
/// standalone call writes. A batch carrying anything else is a user batch and
/// is not this frame.
fn is_create_rule_frame(rec: &WalRecord) -> bool {
    match rec {
        WalRecord::CreateRule { .. } => true,
        WalRecord::Batch(inner) => {
            inner
                .iter()
                .any(|r| matches!(r, WalRecord::CreateRule { .. }))
                && inner
                    .iter()
                    .all(|r| matches!(r, WalRecord::CreateRule { .. } | WalRecord::Intern { .. }))
        }
        _ => false,
    }
}

fn event_from_record(rec: &WalRecord, intern: &Interner, ids: &IdMap) -> Option<MutationEvent> {
    match rec {
        WalRecord::InsertNode { label, key, .. } => Some(MutationEvent::NodeInserted {
            label: label.clone(),
            key: key.clone(),
        }),
        WalRecord::InsertNodeId { label, key, .. } => Some(MutationEvent::NodeInserted {
            label: intern.resolve(*label)?.to_string(),
            key: key.clone(),
        }),
        WalRecord::SetProp { key, field, .. } => Some(MutationEvent::PropSet {
            key: key.clone(),
            field: field.clone(),
        }),
        WalRecord::SetPropId { id, field, .. } => Some(MutationEvent::PropSet {
            key: ids.key_of(*id)?.to_string(),
            field: intern.resolve(*field)?.to_string(),
        }),
        WalRecord::RemoveProp { key, field } => Some(MutationEvent::PropRemoved {
            key: key.clone(),
            field: field.clone(),
        }),
        WalRecord::InsertEdge {
            edge_type,
            src_key,
            dst_key,
        } => Some(MutationEvent::EdgeInserted {
            edge_type: edge_type.clone(),
            src: src_key.clone(),
            dst: dst_key.clone(),
        }),
        WalRecord::InsertEdgeId { etype, src, dst } => Some(MutationEvent::EdgeInserted {
            edge_type: intern.resolve(*etype)?.to_string(),
            src: ids.key_of(*src)?.to_string(),
            dst: ids.key_of(*dst)?.to_string(),
        }),
        WalRecord::DeleteEdge {
            edge_type,
            src_key,
            dst_key,
        } => Some(MutationEvent::EdgeDeleted {
            edge_type: edge_type.clone(),
            src: src_key.clone(),
            dst: dst_key.clone(),
        }),
        WalRecord::DeleteNode { key } => Some(MutationEvent::NodeDeleted { key: key.clone() }),
        WalRecord::CreateRule { def_bytes } => {
            let def: RuleDef = decode_rule_def(def_bytes).ok()?;
            Some(MutationEvent::RuleCreated { name: def.name })
        }
        WalRecord::DeleteRule { name } => Some(MutationEvent::RuleDeleted { name: name.clone() }),
        WalRecord::RebuildRule { name } => Some(MutationEvent::RuleRebuilt { name: name.clone() }),
        WalRecord::Batch(_)
        | WalRecord::CreateView { .. }
        | WalRecord::DeleteView { .. }
        | WalRecord::EnableFulltext { .. }
        | WalRecord::DisableFulltext { .. }
        | WalRecord::EnableIndex { .. }
        | WalRecord::DisableIndex { .. }
        | WalRecord::CreateConstraint { .. }
        | WalRecord::DropConstraint { .. }
        | WalRecord::EnableRangeIndex { .. }
        | WalRecord::DisableRangeIndex { .. }
        | WalRecord::Intern { .. }
        // History markers are no-ops for mutation events — they carry no new
        // state and rules re-derive deterministically on replay.
        | WalRecord::DerivedEdgeAdded { .. }
        | WalRecord::DerivedEdgeRetracted { .. }
        // A count changes neither the node nor the edge population: the pair it
        // counts was already there, which is why it is written at all.
        | WalRecord::SetEdgeCount { .. }
        // RenameNode carries no node/edge count change; no special event.
        | WalRecord::RenameNode { .. } => None,
    }
}

/// Whether `rec` only declares or withdraws something about the store — a
/// full-text pair, an equality or range index, a unique constraint, the
/// multiplicity opt-in — and writes no
/// node, edge or property. These are the records a truncating snapshot
/// re-emits into its baseline WAL.
pub(crate) fn is_declaration(rec: &WalRecord) -> bool {
    matches!(
        rec,
        WalRecord::EnableFulltext { .. }
            | WalRecord::DisableFulltext { .. }
            | WalRecord::EnableIndex { .. }
            | WalRecord::DisableIndex { .. }
            | WalRecord::CreateConstraint { .. }
            | WalRecord::DropConstraint { .. }
            | WalRecord::EnableRangeIndex { .. }
            | WalRecord::DisableRangeIndex { .. }
    ) || rec.is_multiplicity_decl()
}

/// Database-wide counters plus per-rule budget/fire stats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub nodes_live: usize,
    pub nodes_tombstoned: usize,
    pub edges: u64,
    pub rules: Vec<RuleStats>,
    /// How many writes hit the rule-chaining depth cap with work still pending,
    /// since this handle was opened. Non-zero means some derived edges beyond
    /// the cap are stale and no single later write will repair them: split the
    /// rule chain or shorten it. Never persisted, so it resets on reopen.
    #[serde(default)]
    pub chain_truncations: u64,
    /// The oldest commit index history still reaches (the WAL horizon floor).
    /// `0` means nothing has been pruned and history is complete; a non-zero
    /// value means events before that commit were pruned and are gone.
    #[serde(default)]
    pub history_floor: u64,
    /// Live node counts per namespace, in name order. Always carries
    /// `default` — a store is at least its default namespace — so a
    /// single-tenant store reads `[{"name":"default", …}]` and a reader can
    /// tell "no namespaces in use" from one entry.
    #[serde(default)]
    pub namespaces: Vec<NamespaceStats>,
}

/// Live node count for one namespace; one entry of [`Stats::namespaces`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NamespaceStats {
    pub name: String,
    pub nodes_live: usize,
}

/// One rule's provenance size, trip latch, and fire counter.
///
/// `tripped` is a one-way latch: once set, the engine adds no new edges for
/// that rule until [`GraphDb::rebuild_rule`] (and only if the full desired
/// set then fits). `fires` counts `on_node_changed` evaluations plus
/// backfill/rebuild participant ticks (rebuild counts even when it is a
/// provenance no-op).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleStats {
    pub name: String,
    pub edges: u64,
    pub tripped: bool,
    pub fires: u64,
    /// Whether this rule uses the approximate IVF-Flat candidate path.
    pub approximate: bool,
    /// `Some` while this rule's vector index is still being built.
    ///
    /// The rule derives **no** edges until it is `None`: the backfill is one
    /// commit that runs after the index is whole, so a caller never sees a
    /// partial edge set. Absent from the JSON when the rule is not building,
    /// which is every rule created over a corpus at or below
    /// [`core_rules::HNSW_BUILD_BATCH`] vectors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub building: Option<BuildProgress>,
}

/// One entry in the slow-query ring buffer.
#[derive(Debug, Clone, Serialize)]
pub struct SlowQueryEntry {
    /// Execution time in whole milliseconds.
    pub ms: u64,
    /// The Cypher query string that was slow.
    pub query: String,
    /// The commit sequence number at the time the query ran.
    pub at_commit: u64,
}

/// Snapshot of the slow-query log returned by [`GraphDb::slow_query_snapshot`].
#[derive(Debug, Clone, Serialize)]
pub struct SlowQuerySnapshot {
    /// Current threshold in milliseconds (0 = disabled).
    pub threshold_ms: u64,
    /// Total number of slow queries ever recorded (not capped by ring size).
    pub count: u64,
    /// Most-recent slow queries (up to 16), oldest first.
    pub last: Vec<SlowQueryEntry>,
}

/// Internal ring-buffer state protected by a `Mutex` so `query(&self)` can
/// write to it without a mutable borrow.
pub(crate) struct SlowQueryLog {
    entries: std::collections::VecDeque<SlowQueryEntry>,
    total: u64,
}

/// Maximum number of entries kept in the slow-query ring buffer.
const SLOW_QUERY_RING_CAP: usize = 16;

/// Where an unmasked read query reports itself when it is slow: the handle's
/// threshold and ring buffer. A [`crate::reader::ReaderSnapshot`] carries one,
/// so a query run on the lock-free reader lands in the same `/metrics` ring a
/// query run under the read lock does.
#[derive(Clone)]
pub(crate) struct SlowQuerySink {
    threshold_ms: u64,
    log: Arc<std::sync::Mutex<SlowQueryLog>>,
}

impl SlowQuerySink {
    /// The ring and threshold as [`GraphDb::slow_query_snapshot`] reports them.
    pub(crate) fn snapshot(&self) -> SlowQuerySnapshot {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        SlowQuerySnapshot {
            threshold_ms: self.threshold_ms,
            count: log.total,
            last: log.entries.iter().cloned().collect(),
        }
    }

    /// Record `cypher` if `elapsed_ms` reaches the threshold.
    pub(crate) fn record(&self, elapsed_ms: u64, cypher: &str, at_commit: u64) {
        let threshold = self.threshold_ms;
        if threshold > 0 && elapsed_ms >= threshold {
            eprintln!("[mushroomdb] slow query ({elapsed_ms}ms): {cypher}");
            let entry = SlowQueryEntry {
                ms: elapsed_ms,
                query: cypher.to_string(),
                at_commit,
            };
            if let Ok(mut log) = self.log.lock() {
                if log.entries.len() == SLOW_QUERY_RING_CAP {
                    log.entries.pop_front();
                }
                log.entries.push_back(entry);
                log.total += 1;
            }
        }
    }
}

/// Whether `rec` declares or withdraws an index or a constraint, inside a
/// batch or not (see [`is_declaration`]).
fn declares_schema(rec: &WalRecord) -> bool {
    match rec {
        WalRecord::Batch(inner) => inner.iter().any(declares_schema),
        rec => is_declaration(rec),
    }
}

/// Wire summary of a [`Predicate`]. JSON only — `Explanation` is never
/// bincode-persisted (WAL/snapshots store `RuleDef` bytes, not this type).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredicateSummary {
    pub kind: String,
    pub fields: Vec<String>,
    pub min: Option<f64>,
    pub tolerance: Option<f64>,
    pub km: Option<f64>,
    pub parts: Option<Vec<PredicateSummary>>,
    /// True when the owning rule has `approximate=true` (IVF-Flat candidate path).
    /// Always false for predicates reported without rule context (sub-predicates in `parts`).
    #[serde(default)]
    pub approximate: bool,
}

impl From<&Predicate> for PredicateSummary {
    fn from(p: &Predicate) -> Self {
        match p {
            Predicate::KeyMatch { field } => PredicateSummary {
                kind: "key_match".into(),
                fields: vec![field.clone()],
                min: None,
                tolerance: None,
                km: None,
                parts: None,
                approximate: false,
            },
            Predicate::FieldEqual { field } => PredicateSummary {
                kind: "field_equal".into(),
                fields: vec![field.clone()],
                min: None,
                tolerance: None,
                km: None,
                parts: None,
                approximate: false,
            },
            Predicate::Overlap { field, min } => PredicateSummary {
                kind: "overlap".into(),
                fields: vec![field.clone()],
                min: Some(*min),
                tolerance: None,
                km: None,
                parts: None,
                approximate: false,
            },
            Predicate::NumericWithin { field, tolerance } => PredicateSummary {
                kind: "numeric_within".into(),
                fields: vec![field.clone()],
                min: None,
                tolerance: Some(*tolerance),
                km: None,
                parts: None,
                approximate: false,
            },
            Predicate::GeoRadius { field, km } => PredicateSummary {
                kind: "geo_radius".into(),
                fields: vec![field.clone()],
                min: None,
                tolerance: None,
                km: Some(*km),
                parts: None,
                approximate: false,
            },
            Predicate::VectorSimilar { field, min } => PredicateSummary {
                kind: "vector_similar".into(),
                fields: vec![field.clone()],
                min: Some(*min),
                tolerance: None,
                km: None,
                parts: None,
                approximate: false,
            },
            Predicate::All(inner) => {
                let parts: Vec<PredicateSummary> = inner.iter().map(Self::from).collect();
                let mut fields = Vec::new();
                for part in &parts {
                    for f in &part.fields {
                        if !fields.contains(f) {
                            fields.push(f.clone());
                        }
                    }
                }
                PredicateSummary {
                    kind: "all".into(),
                    fields,
                    min: None,
                    tolerance: None,
                    km: None,
                    parts: Some(parts),
                    approximate: false,
                }
            }
            Predicate::Any(inner) => {
                let parts: Vec<PredicateSummary> = inner.iter().map(Self::from).collect();
                let mut fields = Vec::new();
                for part in &parts {
                    for f in &part.fields {
                        if !fields.contains(f) {
                            fields.push(f.clone());
                        }
                    }
                }
                PredicateSummary {
                    kind: "any".into(),
                    fields,
                    min: None,
                    tolerance: None,
                    km: None,
                    parts: Some(parts),
                    approximate: false,
                }
            }
        }
    }
}

/// Snapshot of a live node's key, label, and columnar properties.
///
/// `props` is a [`BTreeMap`] so field order is deterministic (sorted by name)
/// regardless of insert order or the columnar store's `HashMap` iteration.
///
/// Deliberately does not derive `Serialize`: `Value`'s serde form is
/// internally tagged. Wire JSON is built by `value_to_json` in the server.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeInfo {
    pub key: String,
    pub label: String,
    pub props: BTreeMap<String, Value>,
}

/// Counts returned by [`GraphDb::delete_node`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeleteReport {
    /// Number of manual (user-inserted) edges removed.
    pub manual_edges: u64,
    /// Number of derived (rule-owned) edges retracted.
    pub derived_edges: u64,
}

/// One directed edge incident on a node, with provenance membership.
///
/// `derived` is true iff `(edge_type, src, dst)` is in the rule engine's
/// Plan-8 `by_node` provenance index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EdgeInfo {
    pub edge_type: String,
    pub src_key: String,
    pub dst_key: String,
    pub derived: bool,
}

/// One directed edge incident on a node at a point in WAL history, with the
/// rule that derived it when it is rule-owned.
///
/// Returned by [`GraphDb::edges_at`] (sorted by `(edge_type, src_key, dst_key)`)
/// and by [`GraphDb::what_if_set_prop`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct EdgeAt {
    pub edge_type: String,
    pub src_key: String,
    pub dst_key: String,
    /// `true` when a rule wrote the edge (`DerivedEdgeAdded` in the WAL, or a
    /// live provenance entry).
    pub derived: bool,
    /// The rule that derived the edge. `None` for a manual edge.
    pub rule: Option<String>,
}

/// The derived edges a hypothetical property change would retract and derive.
///
/// Returned by [`GraphDb::what_if_set_prop`]. Both lists are sorted by
/// `(edge_type, src_key, dst_key)` and every entry is rule-derived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WhatIf {
    /// Derived edges that exist now and would be retracted.
    pub lost: Vec<EdgeAt>,
    /// Derived edges that do not exist now and would be derived.
    pub gained: Vec<EdgeAt>,
}

/// An edge with mask-aware endpoint visibility.
///
/// Returned by [`GraphDb::node_edges_masked`] in [`crate::mask::MaskMode::Stub`]
/// mode — hidden endpoints carry `*_restricted: true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskedEdge {
    pub edge_type: String,
    pub src_key: String,
    /// `true` when `src_key` is in the DB but hidden from the mask.
    pub src_restricted: bool,
    pub dst_key: String,
    /// `true` when `dst_key` is in the DB but hidden from the mask.
    pub dst_restricted: bool,
    pub derived: bool,
}

/// Result of a mask-aware node lookup via [`GraphDb::node_info_masked`].
///
/// `None` from that method means the key does not exist (→ 404).
/// `Some(Restricted)` is only produced when `mask.mode() == MaskMode::Stub`.
#[derive(Debug, PartialEq)]
pub enum MaskedNodeResult {
    Visible(NodeInfo),
    /// Node exists in the DB but is hidden from this mask.
    Restricted,
}

/// One rule-owned edge between two nodes, with the rule name, edge type,
/// direction (src_key → dst_key), and weight if the rule stores one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Explanation {
    pub rule: String,
    pub edge_type: String,
    pub src_key: String,
    pub dst_key: String,
    pub weight: Option<f64>,
    pub predicate: PredicateSummary,
    /// For a via-hop rule, the edge type the rule hops over to reach its
    /// candidates. `None` for a plain two-node rule. A via-hop rule whose
    /// `via_edge` is itself rule-derived is the chaining case: the hop edge
    /// was written by another rule in the same commit.
    #[serde(default)]
    pub via_edge: Option<String>,
}

/// Report returned by [`GraphDb::backup_to`].
#[derive(Debug, Clone)]
pub struct BackupReport {
    /// Filenames copied into the destination directory (sorted ascending).
    pub files: Vec<String>,
    /// Total bytes written across all copied files.
    pub bytes: u64,
    /// `true` when the destination opened cleanly and passed post-copy checks.
    ///
    /// For stores that have a `snapshot.bin` this means: all V8 section CRCs
    /// matched **and** the destination opened without error.
    ///
    /// For WAL-only stores (no `snapshot.bin`) there is no snapshot to
    /// CRC-check; `verified` is `true` when the destination opened and
    /// replayed the WAL without error (record-level checksums in the WAL
    /// provide the integrity signal, not section CRCs).
    pub verified: bool,
}

/// One directed edge in export form, with optional rule attribution for derived edges.
///
/// Returned by [`GraphDb::all_edges_for_export`].
///
/// Does not derive `Eq`/`Ord`: `weight` is an `f64` and NaN breaks a total
/// order. Callers that need a stable edge ordering already sort by
/// `(edge_type, src, dst)` explicitly (see `all_edges_for_export`).
#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub struct ExportEdge {
    pub edge_type: String,
    pub src: String,
    pub dst: String,
    pub derived: bool,
    /// Rule name that created this edge, if derived. `None` for manual edges.
    pub rule: Option<String>,
    /// The creating rule's declared `weight_prop`, read off this edge, when
    /// derived and numeric (`Int`/`Float`). `None` for manual edges, derived
    /// edges whose rule declares no `weight_prop`, or a non-numeric value.
    pub weight: Option<f64>,
}

/// One edge type's shape, as [`GraphDb::edge_type_census`] counts it.
///
/// Deliberately per *type* and not per edge: everything here is a summary a
/// caller can print in one line, and none of it costs a record per edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeTypeCensus {
    pub edge_type: String,
    /// Directed edges of this type. Counted the way
    /// [`GraphDb::edge_count`] counts: each edge once, from its source.
    pub edges: u64,
    /// Every label seen on a source of this type, sorted.
    pub src_labels: Vec<String>,
    /// Every label seen on a destination of this type, sorted.
    pub dst_labels: Vec<String>,
    /// The rules that declare this `edge_type`, sorted. Empty for a type
    /// written by hand.
    pub rules: Vec<String>,
    /// `(src key, dst key)` of the first edge of this type in the store's own
    /// id order — a real pair to quote in an example.
    pub sample: Option<(String, String)>,
}

/// True when a node's stored `id` property is its own key: the convention
/// `CREATE` and the default ingest follow. A node whose `id` differs from the
/// key, or that has none, is not covered by the key-property rule.
pub(crate) fn id_mirrors_key(key: &str, stored_id: Option<&Value>) -> bool {
    matches!(stored_id, Some(Value::Str(s)) if s == key)
}

/// The refusal for a write that would break the `id`-equals-key correspondence
/// of a node that has it: only `rename_node` moves both together. Raised at
/// `rewrite_wal_dense_inner` (set) and `prepare_remove_prop` (removal), so
/// every API gets it.
pub(crate) fn key_property_error(key: &str) -> GraphError {
    GraphError::QueryError {
        detail: format!(
            "cannot change the key property `id` of node {key}; \
             renaming a node is rename_node"
        ),
    }
}

/// The refusal for removing the key property (`REMOVE n.id`, a null `id`) of a
/// node whose `id` is its key.
pub(crate) fn key_property_removal_error() -> GraphError {
    GraphError::QueryError {
        detail: "cannot REMOVE the key property `id`; renaming a node is rename_node".into(),
    }
}

/// `Err` when setting `id` to `new` on the node `key`, whose stored `id` is
/// `stored_id`, would break `id == key`. The engine enforces this at its write
/// choke point; a transaction calls it to refuse at the statement, not at
/// commit.
pub(crate) fn check_key_property_write(
    key: &str,
    stored_id: Option<&Value>,
    new: Option<&Value>,
) -> Result<()> {
    if id_mirrors_key(key, stored_id) && !id_mirrors_key(key, new) {
        return Err(key_property_error(key));
    }
    Ok(())
}

/// `Err` when removing `id` from the node `key` whose stored `id` is `stored_id`
/// would break `id == key`.
pub(crate) fn check_key_property_removal(key: &str, stored_id: Option<&Value>) -> Result<()> {
    if id_mirrors_key(key, stored_id) {
        return Err(key_property_removal_error());
    }
    Ok(())
}

/// A SET map's keys with each value evaluated for one matched row; `None` is null.
type MapEntries = Vec<(String, Option<Value>)>;

/// One property write a Cypher `SET n = {…}` / `SET n += {…}` plans for a node.
#[derive(Debug, Clone, PartialEq)]
enum MapWrite {
    Set { field: String, value: Value },
    Remove { field: String },
}

/// The writes `SET n = map` (`merge == false`) or `SET n += map` (`merge ==
/// true`) makes to the node `key`, whose current properties are `existing`.
/// `entries` pairs each map key with its evaluated value; `None` is null.
///
/// Decisions, each pinned by a test:
/// - `+=`: a non-null value sets the property; a null value removes it (a
///   no-op when the node lacks it); every other property is left alone.
/// - `=`: the node's properties become exactly the non-null entries. A null
///   entry is an absent key, so it removes the property if the node has it.
///   Removals are listed before sets in the batch.
/// - `id`: on a node whose `id` is its key, `=` never removes it and a map that
///   names it writes it like any property; the engine's write choke point
///   decides (the key passes, any other value or null is refused — a key change
///   is `rename_node`). On a node whose `id` differs from its key, or that has
///   none, it is an ordinary property.
/// - `ns` is never removed by `=` (a map without it keeps the namespace). A
///   map that sets it is checked by the batch (`NamespaceImmutable` for a
///   different one); `+= {ns: null}` is a removal, refused there the same way.
/// - A view-owned property is never removed by `=` (`is_view_owned`), the same
///   filter `on_conflict="replace"` applies. Naming one in the map, with a value
///   or null, reaches the batch and is `ViewPropReadOnly`.
fn plan_map_write(
    key: &str,
    existing: &BTreeMap<String, Value>,
    merge: bool,
    entries: &[(String, Option<Value>)],
    is_view_owned: &dyn Fn(&str) -> bool,
) -> Vec<MapWrite> {
    let mut sets = Vec::new();
    let mut removes = Vec::new();
    for (field, value) in entries {
        match value {
            Some(v) => sets.push(MapWrite::Set {
                field: field.clone(),
                value: v.clone(),
            }),
            None => removes.push(MapWrite::Remove {
                field: field.clone(),
            }),
        }
    }
    if !merge {
        let kept: BTreeSet<&str> = entries
            .iter()
            .filter(|(_, v)| v.is_some())
            .map(|(f, _)| f.as_str())
            .collect();
        // A null entry is already a removal above; do not plan it twice.
        for field in existing.keys() {
            let protected = (field == "id" && id_mirrors_key(key, existing.get("id")))
                || field == NS_PROP
                || is_view_owned(field);
            let already = removes
                .iter()
                .any(|w| matches!(w, MapWrite::Remove { field: f } if f == field));
            if !protected && !kept.contains(field.as_str()) && !already {
                removes.push(MapWrite::Remove {
                    field: field.clone(),
                });
            }
        }
    }
    removes.extend(sets);
    removes
}

/// Construct the standard write-query result set (columns: created, properties_set, deleted).
pub(crate) fn write_result_set() -> ResultSet {
    ResultSet::new(vec![
        "created".into(),
        "properties_set".into(),
        "deleted".into(),
    ])
}

pub(crate) fn resolve_merge_set_value(
    op: &Operand,
    params: &BTreeMap<String, Value>,
) -> Result<Value> {
    match op {
        Operand::Lit(v) => Ok(v.clone()),
        Operand::Param(name) => params
            .get(name)
            .cloned()
            .ok_or_else(|| GraphError::QueryError {
                detail: format!("missing parameter `{name}`"),
            }),
        _ => Err(GraphError::QueryError {
            detail: "ON CREATE/ON MATCH SET value must be a literal or $parameter".into(),
        }),
    }
}

fn operand_node_vars(op: &Operand, out: &mut Vec<String>) {
    match op {
        Operand::Prop { var, .. } | Operand::Var(var) => {
            if !out.contains(var) {
                out.push(var.clone());
            }
        }
        Operand::FuncCall { args, .. } => {
            for arg in args {
                operand_node_vars(arg, out);
            }
        }
        Operand::BinArith { left, right, .. } => {
            operand_node_vars(left, out);
            operand_node_vars(right, out);
        }
        Operand::Case { branches, default } => {
            // Branch conditions reference vars already bound (and mask-filtered)
            // by the MATCH phase, so collecting from the value operands + ELSE
            // is sufficient for RETURN-projection var discovery.
            for (_, value) in branches {
                operand_node_vars(value, out);
            }
            if let Some(d) = default {
                operand_node_vars(d, out);
            }
        }
        Operand::Index { base, index } => {
            operand_node_vars(base, out);
            operand_node_vars(index, out);
        }
        Operand::ListLit(items) => {
            for item in items {
                operand_node_vars(item, out);
            }
        }
        Operand::MapLit(entries) => {
            for (_, value) in entries {
                operand_node_vars(value, out);
            }
        }
        Operand::ListComprehension { list, .. } => operand_node_vars(list, out),
        Operand::Reduce { init, list, .. } => {
            operand_node_vars(init, out);
            operand_node_vars(list, out);
        }
        Operand::Pred(expr) => expr_node_vars(expr, out),
        Operand::Lit(_) | Operand::Param(_) => {}
    }
}

/// [`operand_node_vars`] for every operand of a predicate.
fn expr_node_vars(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) => {
            expr_node_vars(a, out);
            expr_node_vars(b, out);
        }
        Expr::Not(e) => expr_node_vars(e, out),
        Expr::Cmp { lhs, rhs, .. } => {
            operand_node_vars(lhs, out);
            operand_node_vars(rhs, out);
        }
        Expr::Truthy(o) | Expr::IsNull(o) | Expr::IsNotNull(o) => operand_node_vars(o, out),
        Expr::In { expr, list } => {
            operand_node_vars(expr, out);
            operand_node_vars(list, out);
        }
    }
}

pub(crate) fn ret_node_vars(items: &[RetItem]) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        match &item.value {
            RetVal::Var(v) | RetVal::Prop { var: v, .. } => {
                if !out.contains(v) {
                    out.push(v.clone());
                }
            }
            RetVal::FuncCall { args, .. } => {
                for arg in args {
                    operand_node_vars(arg, &mut out);
                }
            }
            RetVal::ScalarExpr(op) => operand_node_vars(op, &mut out),
            RetVal::Agg { .. } => {}
        }
    }
    out
}

pub(crate) fn add_var(out: &mut Vec<String>, v: &str) {
    if !out.iter().any(|x| x == v) {
        out.push(v.to_string());
    }
}

pub(crate) fn pattern_node_vars(pats: &[Pattern]) -> Vec<String> {
    let mut out = Vec::new();
    for p in pats {
        if let Some(v) = &p.start.var {
            add_var(&mut out, v);
        }
        for (_, dest) in &p.chain {
            if let Some(v) = &dest.var {
                add_var(&mut out, v);
            }
        }
    }
    out
}

pub(crate) fn pattern_rel_vars(pats: &[Pattern]) -> Vec<String> {
    let mut out = Vec::new();
    for p in pats {
        for (rel, _) in &p.chain {
            if rel.hops.is_none() {
                if let Some(v) = &rel.var {
                    add_var(&mut out, v);
                }
            }
        }
    }
    out
}

pub(crate) fn rel_type_alias(var: &str) -> String {
    format!("__rt_{var}")
}

fn ret_column_name(item: &RetItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    // The same naming rule the planner and the executor use, so a
    // write-statement RETURN names its columns exactly as a read query does.
    // An aggregate is not legal in a write-statement RETURN; it keeps the
    // placeholder it always had.
    ret_val_label(&item.value).unwrap_or_else(|| "<agg>".to_string())
}

/// The refusal for a list comprehension in a write statement's RETURN.
const COMPREHENSION_IN_WRITE_RETURN: &str =
    "a list comprehension is not supported in the RETURN of a write statement; \
     compute it in a read query, or RETURN the list and filter it afterwards";

/// The refusal for `reduce()` in a write statement's RETURN.
const REDUCE_IN_WRITE_RETURN: &str =
    "reduce() is not supported in the RETURN of a write statement; \
     compute it in a read query, or RETURN the list and fold it afterwards";

/// The refusal for the first scoped-variable construct (a list comprehension
/// or `reduce()`) found in an expression, if any.
fn expr_write_return_refusal(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) => {
            expr_write_return_refusal(a).or_else(|| expr_write_return_refusal(b))
        }
        Expr::Not(e) => expr_write_return_refusal(e),
        Expr::Cmp { lhs, rhs, .. } => {
            operand_write_return_refusal(lhs).or_else(|| operand_write_return_refusal(rhs))
        }
        Expr::Truthy(o) | Expr::IsNull(o) | Expr::IsNotNull(o) => operand_write_return_refusal(o),
        Expr::In { expr, list } => {
            operand_write_return_refusal(expr).or_else(|| operand_write_return_refusal(list))
        }
    }
}

fn operand_write_return_refusal(op: &Operand) -> Option<&'static str> {
    match op {
        Operand::ListComprehension { .. } => Some(COMPREHENSION_IN_WRITE_RETURN),
        Operand::Reduce { .. } => Some(REDUCE_IN_WRITE_RETURN),
        Operand::FuncCall { args, .. } => args.iter().find_map(operand_write_return_refusal),
        Operand::BinArith { left, right, .. } => {
            operand_write_return_refusal(left).or_else(|| operand_write_return_refusal(right))
        }
        Operand::Index { base, index } => {
            operand_write_return_refusal(base).or_else(|| operand_write_return_refusal(index))
        }
        Operand::ListLit(items) => items.iter().find_map(operand_write_return_refusal),
        Operand::MapLit(entries) => entries
            .iter()
            .find_map(|(_, v)| operand_write_return_refusal(v)),
        Operand::Case { branches, default } => branches
            .iter()
            .find_map(|(c, v)| {
                expr_write_return_refusal(c).or_else(|| operand_write_return_refusal(v))
            })
            .or_else(|| default.as_deref().and_then(operand_write_return_refusal)),
        Operand::Pred(expr) => expr_write_return_refusal(expr),
        Operand::Prop { .. } | Operand::Lit(_) | Operand::Param(_) | Operand::Var(_) => None,
    }
}

fn returns_write_return_refusal(returns: &[RetItem]) -> Option<&'static str> {
    returns.iter().find_map(|item| match &item.value {
        RetVal::FuncCall { args, .. } => args.iter().find_map(operand_write_return_refusal),
        RetVal::ScalarExpr(op) => operand_write_return_refusal(op),
        _ => None,
    })
}

fn eval_set_return_operand<F: Fs>(
    db: &GraphDb<F>,
    match_rs: &ResultSet,
    row: usize,
    rel_vars: &[String],
    op: &Operand,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Value>> {
    match op {
        // `null` parses to `Lit(Value::Null)`; as a query value it is `None`.
        Operand::Lit(Value::Null) => Ok(None),
        Operand::Lit(v) => Ok(Some(v.clone())),
        Operand::Param(name) => params.get(name).cloned().ok_or_else(|| GraphError::QueryError {
            detail: format!("missing parameter `{name}`"),
        }).map(core_query::value_ops::element),
        Operand::Var(name) if rel_vars.iter().any(|r| r == name) => Err(GraphError::QueryError {
            detail: format!(
                "cannot return relationship variable '{name}' bare; return its properties ({name}.field) instead"
            ),
        }),
        Operand::Var(name) => Ok(match_rs.get(row, name).cloned()),
        Operand::Prop { var, field } => {
            if rel_vars.iter().any(|r| r == var) {
                return Ok(None);
            }
            let Some(Value::Str(key)) = match_rs.get(row, var) else {
                return Ok(None);
            };
            if let Some(v) = db.get_prop(key, field) {
                return Ok(Some(v));
            }
            // Same stored-wins identity fallback as the read path:
            // n.key / n.id / n.label, not only get_prop.
            Ok(match field.as_str() {
                "key" | "id" => Some(Value::Str(key.clone())),
                "label" => db
                    .node_ref(key)
                    .map(|n| Value::Str(n.label().to_owned())),
                _ => None,
            })
        }
        Operand::FuncCall { name, args } => {
            eval_set_return_func(db, match_rs, row, rel_vars, name, args, params)
        }
        Operand::BinArith { op, left, right } => {
            let lv = eval_set_return_operand(db, match_rs, row, rel_vars, left, params)?;
            let rv = eval_set_return_operand(db, match_rs, row, rel_vars, right, params)?;
            eval_set_return_arith(op, lv, rv)
        }
        Operand::Case { branches, default } => {
            for (cond, value) in branches {
                if eval_set_return_expr(db, match_rs, row, rel_vars, cond, params)? {
                    return eval_set_return_operand(db, match_rs, row, rel_vars, value, params);
                }
            }
            match default {
                Some(d) => eval_set_return_operand(db, match_rs, row, rel_vars, d, params),
                None => Ok(None),
            }
        }
        Operand::Index { base, index } => {
            let base_val = eval_set_return_operand(db, match_rs, row, rel_vars, base, params)?;
            let idx_val = eval_set_return_operand(db, match_rs, row, rel_vars, index, params)?;
            Ok(core_query::value_ops::index_list(base_val, idx_val))
        }
        // A null element or value is kept, as `Value::Null`.
        Operand::ListLit(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let v = eval_set_return_operand(db, match_rs, row, rel_vars, item, params)?;
                out.push(core_query::value_ops::into_element(v));
            }
            Ok(Some(Value::List(out)))
        }
        // Refused by name before the write commits (`exec_match_set`); this
        // arm is only a backstop and never sees a comprehension.
        Operand::ListComprehension { .. } => Err(GraphError::QueryError {
            detail: COMPREHENSION_IN_WRITE_RETURN.into(),
        }),
        Operand::Reduce { .. } => Err(GraphError::QueryError {
            detail: REDUCE_IN_WRITE_RETURN.into(),
        }),
        Operand::MapLit(entries) => {
            let mut out = BTreeMap::new();
            for (key, value) in entries {
                let v = eval_set_return_operand(db, match_rs, row, rel_vars, value, params)?;
                out.insert(key.clone(), core_query::value_ops::into_element(v));
            }
            Ok(Some(Value::Map(out)))
        }
        Operand::Pred(expr) => {
            let value = core_query::cypher::eval_predicate(
                expr,
                &mut |o| eval_set_return_operand(db, match_rs, row, rel_vars, o, params),
                &|detail| GraphError::QueryError { detail },
            )?;
            Ok(value.map(Value::Bool))
        }
    }
}

/// The start of the error a write gets when a property value holds a null.
const NULL_PROPERTY_ERROR: &str = "a property value cannot contain null";

/// A stored property is never null, at any depth: `[1, null]` and
/// `{a: null}` exist only as query results. Checked on the dense-rewrite
/// seam every node insert and property write passes through (contract C2),
/// before anything reaches the WAL. Storing a list or map property that holds
/// a null is not supported in 0.10.
fn refuse_null_property(key: &str, field: &str, value: &Value) -> Result<()> {
    if value.contains_null() {
        return Err(GraphError::QueryError {
            detail: format!(
                "{NULL_PROPERTY_ERROR}: property `{field}` of node `{key}` holds a null \
                 (a list or map property with a null element cannot be stored)"
            ),
        });
    }
    Ok(())
}

/// A CASE WHEN condition in a write statement's RETURN: true takes the
/// branch; false and null do not. Three-valued, as in a read query.
fn eval_set_return_expr<F: Fs>(
    db: &GraphDb<F>,
    match_rs: &ResultSet,
    row: usize,
    rel_vars: &[String],
    expr: &Expr,
    params: &BTreeMap<String, Value>,
) -> Result<bool> {
    let value = core_query::cypher::eval_predicate(
        expr,
        &mut |o| eval_set_return_operand(db, match_rs, row, rel_vars, o, params),
        &|detail| GraphError::QueryError { detail },
    )?;
    Ok(value == Some(true))
}

fn eval_set_return_arith(
    op: &ArithOp,
    lv: Option<Value>,
    rv: Option<Value>,
) -> Result<Option<Value>> {
    match (lv, rv) {
        (None, _) | (_, None) => Ok(None),
        (Some(Value::Int(a)), Some(Value::Int(b))) => {
            let result = match op {
                ArithOp::Sub => a.saturating_sub(b),
                ArithOp::Mul => a.saturating_mul(b),
                ArithOp::Add => a.saturating_add(b),
                ArithOp::Div => {
                    if b == 0 {
                        return Err(GraphError::QueryError {
                            detail: "division by zero".into(),
                        });
                    }
                    a.checked_div(b).unwrap_or(i64::MAX)
                }
                ArithOp::Mod => {
                    if b == 0 {
                        return Err(GraphError::QueryError {
                            detail: "division by zero".into(),
                        });
                    }
                    a.checked_rem(b).unwrap_or(0)
                }
                ArithOp::Pow => return Ok(Some(Value::Float((a as f64).powf(b as f64)))),
            };
            Ok(Some(Value::Int(result)))
        }
        (Some(lv), Some(rv)) => {
            if matches!(op, ArithOp::Add) {
                if let Some(joined) = core_query::value_ops::plus_non_numeric(&lv, &rv) {
                    return Ok(Some(joined));
                }
            }
            let a = match &lv {
                Value::Float(f) => *f,
                Value::Int(i) => *i as f64,
                _ => {
                    return Err(GraphError::QueryError {
                        detail: format!("arithmetic operand must be numeric, got {lv:?}"),
                    })
                }
            };
            let b = match &rv {
                Value::Float(f) => *f,
                Value::Int(i) => *i as f64,
                _ => {
                    return Err(GraphError::QueryError {
                        detail: format!("arithmetic operand must be numeric, got {rv:?}"),
                    })
                }
            };
            let result = match op {
                ArithOp::Sub => a - b,
                ArithOp::Mul => a * b,
                ArithOp::Add => a + b,
                ArithOp::Div => {
                    if b == 0.0 {
                        return Err(GraphError::QueryError {
                            detail: "division by zero".into(),
                        });
                    }
                    a / b
                }
                ArithOp::Mod => a % b,
                ArithOp::Pow => a.powf(b),
            };
            Ok(Some(Value::Float(result)))
        }
    }
}

fn eval_set_return_func<F: Fs>(
    db: &GraphDb<F>,
    match_rs: &ResultSet,
    row: usize,
    rel_vars: &[String],
    name: &str,
    args: &[Operand],
    params: &BTreeMap<String, Value>,
) -> Result<Option<Value>> {
    let norm = name.to_ascii_lowercase();
    if norm == "type" {
        if args.len() != 1 {
            return Err(GraphError::QueryError {
                detail: format!("type() requires exactly 1 argument, got {}", args.len()),
            });
        }
        let Operand::Var(rel) = &args[0] else {
            return Err(GraphError::QueryError {
                detail: "type() argument must be a relationship variable (e.g. type(r))".into(),
            });
        };
        return Ok(match_rs.get(row, &rel_type_alias(rel)).cloned());
    }
    if norm == "key" || norm == "id" {
        let fname = if norm == "id" { "id" } else { "key" };
        if args.len() != 1 {
            return Err(GraphError::QueryError {
                detail: format!("{fname}() requires exactly 1 argument, got {}", args.len()),
            });
        }
        let Operand::Var(var) = &args[0] else {
            return Err(GraphError::QueryError {
                detail: format!("{fname}() argument must be a node variable (e.g. {fname}(n))"),
            });
        };
        if rel_vars.iter().any(|r| r == var) {
            return Err(GraphError::QueryError {
                detail: format!("{fname}() argument `{var}` is a relationship, not a node"),
            });
        }
        // MATCH rows bind node variables to their key string, so the column
        // value *is* the key. `id()` aliases `key()`.
        return Ok(match_rs.get(row, var).cloned());
    }
    let mut vals = Vec::with_capacity(args.len());
    for arg in args {
        vals.push(eval_set_return_operand(
            db, match_rs, row, rel_vars, arg, params,
        )?);
    }
    match norm.as_str() {
        "tolower" => {
            if vals.len() != 1 {
                return Err(GraphError::QueryError {
                    detail: format!("toLower() requires exactly 1 argument, got {}", vals.len()),
                });
            }
            Ok(vals[0].clone().map(|val| match val {
                Value::Str(s) => Value::Str(s.to_ascii_lowercase()),
                other => other,
            }))
        }
        "toupper" => {
            if vals.len() != 1 {
                return Err(GraphError::QueryError {
                    detail: format!("toUpper() requires exactly 1 argument, got {}", vals.len()),
                });
            }
            Ok(vals[0].clone().map(|val| match val {
                Value::Str(s) => Value::Str(s.to_ascii_uppercase()),
                other => other,
            }))
        }
        "size" => match vals.first().cloned().flatten() {
            None => Ok(None),
            Some(Value::Str(s)) => Ok(Some(Value::Int(s.len() as i64))),
            Some(Value::List(items)) => Ok(Some(Value::Int(items.len() as i64))),
            Some(_) => Ok(None),
        },
        "coalesce" => Ok(vals.into_iter().flatten().next()),
        "abs" => match vals.first().cloned().flatten() {
            None => Ok(None),
            Some(Value::Int(n)) => Ok(Some(Value::Int(n.saturating_abs()))),
            Some(Value::Float(f)) => Ok(Some(Value::Float(f.abs()))),
            Some(_) => Ok(None),
        },
        "round" => match vals.first().cloned().flatten() {
            None => Ok(None),
            Some(Value::Float(f)) => Ok(Some(Value::Float(f.round()))),
            Some(Value::Int(n)) => Ok(Some(Value::Int(n))),
            Some(_) => Ok(None),
        },
        "decay" => {
            if vals.len() != 3 {
                return Err(GraphError::QueryError {
                    detail: format!("decay() requires exactly 3 arguments, got {}", vals.len()),
                });
            }
            match (vals[0].clone(), vals[1].clone(), vals[2].clone()) {
                (None, _, _) | (_, None, _) | (_, _, None) => Ok(None),
                (Some(b), Some(a), Some(h)) => {
                    let numeric = |v: Value| -> Result<f64> {
                        match v {
                            Value::Int(n) => Ok(n as f64),
                            Value::Float(f) => Ok(f),
                            other => Err(GraphError::QueryError {
                                detail: format!(
                                    "decay() requires numeric arguments, got {other:?}"
                                ),
                            }),
                        }
                    };
                    let b = numeric(b)?;
                    let a = numeric(a)?;
                    let h = numeric(h)?;
                    if h <= 0.0 {
                        return Err(GraphError::QueryError {
                            detail: "decay() requires halflife > 0".into(),
                        });
                    }
                    Ok(Some(Value::Float(b * 0.5f64.powf(a / h))))
                }
            }
        }
        _ => Err(GraphError::QueryError {
            detail: format!(
                "unknown function `{name}`; supported: toLower, toUpper, size, coalesce, type, abs, round, decay, key, id"
            ),
        }),
    }
}

fn eval_set_return_item<F: Fs>(
    db: &GraphDb<F>,
    match_rs: &ResultSet,
    row: usize,
    rel_vars: &[String],
    item: &RetItem,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Value>> {
    match &item.value {
        RetVal::Var(v) => eval_set_return_operand(
            db,
            match_rs,
            row,
            rel_vars,
            &Operand::Var(v.clone()),
            params,
        ),
        RetVal::Prop { var, field } => eval_set_return_operand(
            db,
            match_rs,
            row,
            rel_vars,
            &Operand::Prop {
                var: var.clone(),
                field: field.clone(),
            },
            params,
        ),
        RetVal::FuncCall { name, args } => {
            eval_set_return_func(db, match_rs, row, rel_vars, name, args, params)
        }
        RetVal::ScalarExpr(op) => eval_set_return_operand(db, match_rs, row, rel_vars, op, params),
        RetVal::Agg { .. } => Err(GraphError::QueryError {
            detail: "aggregates are not supported in MATCH … SET … RETURN".into(),
        }),
    }
}

/// Project user RETURN from original MATCH rows after SET. No rematch.
fn project_set_return_rows<F: Fs>(
    db: &GraphDb<F>,
    rel_vars: &[String],
    match_rs: &ResultSet,
    returns: &[RetItem],
    params: &BTreeMap<String, Value>,
) -> Result<ResultSet> {
    let columns: Vec<String> = returns.iter().map(ret_column_name).collect();
    let mut out = ResultSet::new(columns);
    for row in 0..match_rs.len() {
        let mut cells = Vec::with_capacity(returns.len());
        for item in returns {
            cells.push(eval_set_return_item(
                db, match_rs, row, rel_vars, item, params,
            )?);
        }
        out.push_row(cells);
    }
    Ok(out)
}

/// Single construction point for a `GraphMut` view over the split-borrowed graph fields.
/// Callers use `std::mem::take` on the engine before calling this, then restore it after.
/// Extract a `Vec<f64>` from a `Value::List` whose items are all numeric.
/// Returns `None` for non-list values or lists with non-numeric elements.
/// Extra candidates pulled from an approximate index before re-scoring, over and
/// above the `k` asked for.
///
/// The index orders candidates by `f32` distances, which agree with the exact
/// `f64` cosine to about 1e-6. Re-scoring can therefore only reshuffle
/// candidates inside a band that narrow — it cannot move a hit past one that is
/// further away by more than 1e-6 — so the only way a true top-`k` member can be
/// lost is if the index ranked it just outside `k` on the `f32` order. Fetching
/// `k + 16` covers any such band up to 16 members wide, which at 1e-6 means 16
/// vectors within a millionth of each other in cosine: a duplicate cluster, and
/// then the members are interchangeable anyway. `min` is applied to the exact
/// score, never to the index's, so a hit sitting on the threshold is decided
/// exactly.
const VECTOR_RESCORE_MARGIN: usize = 16;

/// Cosine similarity between an already-unit query and node `id`'s `field`
/// vector, read from the **`f64`** properties. `None` when the node has no
/// numeric-list vector there, or its norm is zero.
///
/// The single definition of the score this API reports. Both the brute-force
/// scan and the re-scoring step that follows an index lookup go through it, so
/// the two paths cannot disagree — which is the property
/// `index_and_brute_force_agree_on_scores` pins.
fn exact_vector_similarity(
    view: &GraphView<'_>,
    id: u32,
    field: &str,
    q_unit: &[f64],
) -> Option<f64> {
    let v = view.prop(id, field)?;
    let xs = value_as_float_list(&v.into_value())?;
    let v_norm: f64 = xs.iter().map(|x| x * x).sum::<f64>().sqrt();
    if v_norm == 0.0 {
        return None;
    }
    Some(
        q_unit
            .iter()
            .zip(xs.iter())
            .map(|(a, b)| a * (b / v_norm))
            .sum(),
    )
}

fn value_as_float_list(v: &Value) -> Option<Vec<f64>> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Float(f) => Some(*f),
                Value::Int(i) => Some(*i as f64),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// The engine's view of the store. Pass the store's `&mut Arc`s, not
/// `Arc::make_mut` of them: the engine copies a structure the fold shares
/// only when it writes to it (ledger row 126).
fn make_graph_mut<'a>(
    ids: &'a IdMap,
    syms: impl Into<core_rules::CowMut<'a, Interner>>,
    labels: &'a [u32],
    props: core_storage::v8::seam::ColumnsView<'a>,
    topo: impl Into<core_rules::CowMut<'a, Topology>>,
    base: &'a Option<std::sync::Arc<core_storage::v8::MappedBase>>,
    edge_props: impl Into<core_rules::CowMut<'a, EdgeProps>>,
) -> GraphMut<'a> {
    GraphMut {
        ids,
        syms: syms.into(),
        labels,
        props,
        topo: topo.into(),
        base_topo: base_csr(base),
        edge_props: edge_props.into(),
    }
}

/// The archived CSR of an open V8 snapshot, for the rule engine's graph reads.
///
/// A store opened from a snapshot keeps its edges in the mapping and its
/// overlay empty, so a rule that reads the graph's shape has to see both.
fn base_csr(
    base: &Option<std::sync::Arc<core_storage::v8::MappedBase>>,
) -> Option<&core_storage::v8::layout::ArchivedCsr> {
    base.as_ref().map(|b| {
        b.topology()
            .expect("base topology section bounds validated at open")
    })
}

/// Build a `ColumnsView` from the disjoint `props` overlay and optional V8 base.
///
/// Takes explicit field references rather than `&self` so the caller can hold
/// simultaneous mutable borrows of other fields (e.g. `syms`, `topo`).
fn build_props_view<'a>(
    props: &'a ColumnStore,
    base: &'a Option<std::sync::Arc<core_storage::v8::MappedBase>>,
) -> core_storage::v8::seam::ColumnsView<'a> {
    match base {
        None => core_storage::v8::seam::ColumnsView::owned(props),
        Some(b) => {
            let archived = b
                .columns()
                .expect("base columns section bounds validated at open");
            core_storage::v8::seam::ColumnsView::with_base_cached(props, archived, b.mixed_cache())
                .with_shared_strings(base_string_table(b))
        }
    }
}

/// The base columns section paired with the string table that resolves its
/// string ids — what `ViewStore` needs to read a neighbour's string property
/// out of a V9 snapshot.
fn base_columns(
    base: &Option<std::sync::Arc<core_storage::v8::MappedBase>>,
) -> Option<core_storage::v8::seam::BaseColumns<'_>> {
    base.as_ref().map(|b| core_storage::v8::seam::BaseColumns {
        cols: b
            .columns()
            .expect("base columns section bounds validated at open"),
        strings: base_string_table(b),
    })
}

/// The shared string table of a V9 base, or `None` for a pre-V9 one.
///
/// Every `ColumnsView` built over a base must carry it: without it a V9
/// snapshot's string columns, whose own tables are empty, read back as absent.
fn base_string_table(
    base: &core_storage::v8::MappedBase,
) -> Option<&core_storage::v8::layout::ArchivedStringTable> {
    base.string_table()
        .transpose()
        .expect("base strings section bounds validated at open")
}

fn build_topo_view<'a>(
    overlay: &'a Topology,
    base: &'a Option<std::sync::Arc<core_storage::v8::MappedBase>>,
) -> core_storage::v8::seam::TopologyView<'a> {
    match base {
        None => core_storage::v8::seam::TopologyView::owned(overlay),
        Some(b) => {
            let archived_csr = b
                .topology()
                .expect("base topology section bounds validated at open");
            core_storage::v8::seam::TopologyView::with_base(overlay, archived_csr)
        }
    }
}

/// When [`GraphDb`] calls `Fs::sync` after a WAL append.
///
/// Default is [`Strict`](FsyncPolicy::Strict): every `log_then_apply_with`
/// fsyncs (single `insert_node` / `set_prop`). Ingest and `write_batch`
/// emit one `WalRecord::Batch` and fsync once at that frame (Batched).
/// [`Relaxed`](FsyncPolicy::Relaxed) skips WAL sync; [`GraphDb::snapshot`]
/// is still durable via `write_atomic`. Crash-recovery DST stays Strict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FsyncPolicy {
    /// Every WAL commit calls `fs.sync` (today's behavior).
    #[default]
    Strict,
    /// Sync only at a `Batch` frame end. Single-op path stays Strict unless
    /// this policy is set on the database.
    Batched,
    /// Never call `fs.sync`. [`GraphDb::snapshot`] still syncs via `write_atomic`.
    Relaxed,
}

/// A precondition for a compare-and-set batch write.
///
/// All preconditions in a [`GraphDb::write_batch_cas`] or
/// [`crate::SharedDb::submit_batch_cas`] call are checked atomically before
/// any operation in the batch is applied.  If any precondition fails, the
/// entire batch is rejected with [`GraphError::CasConflict`] and no WAL frame
/// is written.
///
/// # Touch definition
///
/// A node's last-change commit (`last_changed`) is updated when any of the
/// following state-changing WAL records touch it:
///
/// - `InsertNode` / `InsertNodeId` — the newly-inserted node.
/// - `SetProp` / `SetPropId` / `RemoveProp` — the property-bearing node.
/// - `InsertEdge` / `InsertEdgeId` / `DeleteEdge` — **both** src and dst
///   endpoints (an edge change touches both sides).
/// - `DeleteNode` — the node is tombstoned; `last_changed` returns `None`
///   for deleted keys so the pre-deletion entry is never observed.
///
/// History markers (`DerivedEdgeAdded` / `DerivedEdgeRetracted`) are
/// state no-ops.  The underlying mutation that triggered rule firing already
/// updated the relevant nodes' last-change entries.  Rule-management records
/// (`CreateRule`, `DeleteRule`, `RebuildRule`) and view/full-text declarations
/// do not touch any node's last-change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// The node's last-change commit must equal `expected`.
    ///
    /// Fails with [`GraphError::CasConflict`] when:
    /// - The node does not exist (`last_changed` returns `None`), or
    /// - The recorded commit seq does not match `expected`.
    NodeUnchangedSince { key: String, expected: u64 },
    /// The node must not exist (not inserted, or already deleted).
    ///
    /// Fails with [`GraphError::CasConflict`] (expected=`u64::MAX`,
    /// actual=`last_changed(key).unwrap_or(0)`) when the node is live.
    NodeAbsent { key: String },
}

pub struct GraphDb<F: Fs> {
    fs: F,
    ids: Arc<IdMap>,
    syms: Arc<Interner>,
    topo: Arc<Topology>,
    props: Arc<ColumnStore>,
    labels: Arc<Vec<u32>>, // node id -> label symbol
    /// Namespace names by index; index [`NS_DEFAULT_IDX`] is always
    /// [`NS_DEFAULT`]. Derived beside [`Self::node_ns`], never persisted.
    ///
    /// A private table rather than the shared [`Interner`]: interning
    /// `"default"` at open would add a symbol to the store's symbol table and
    /// change the bytes of the next snapshot of a store that has no namespaces
    /// at all.
    ns_names: Vec<String>,
    /// Namespace index per dense node id, into [`Self::ns_names`];
    /// [`NS_DEFAULT_IDX`] for a node with no `ns` property.
    ///
    /// Derived: built by one pass over the `ns` column at open (which reads
    /// nothing when the column does not exist) and maintained at every node
    /// insert. Never written to a snapshot or the WAL, because the property it
    /// mirrors already is. A namespace cannot change, so no other record shape
    /// can move a node between namespaces.
    node_ns: Vec<u32>,
    edge_props: Arc<EdgeProps>,
    engine: RuleEngine,
    view_store: ViewStore,
    /// Incremental inverted index for full-text-lite search.
    /// Rebuild-on-open: populated from WAL replay + rebuild_all at open end.
    fulltext: Arc<FulltextIndex>,
    /// Opt-in equality index over scalar node properties.
    /// Rebuild-on-open: declarations replay from the WAL, postings rebuild at
    /// open end (mirrors `fulltext`).
    prop_index: Arc<PropertyIndex>,
    /// Whether this store records insert-count multiplicity (§5.13).
    ///
    /// Declared like `prop_index`'s enabled pairs — a WAL record replayed at
    /// open, re-emitted into the baseline by a truncating snapshot — but it
    /// gates a *format* step rather than an index: `WalRecord::SetEdgeCount`
    /// (discriminant 23) is written only when this is `true`, so a store that
    /// never opts in stays readable by a binary that predates the record.
    multiplicity: bool,
    /// Declared unique constraints, in declaration order.
    ///
    /// Declared like `prop_index`'s enabled pairs — a `CreateConstraint` /
    /// `DropConstraint` record replayed at open, re-emitted into the baseline
    /// by a truncating snapshot — and enforced on new writes only, in
    /// [`rewrite_wal_dense_planned`](Self::rewrite_wal_dense_planned). Replay
    /// never checks them.
    constraints: Vec<UniqueConstraint>,
    /// The `(label, field)` equality indexes this open dropped because a view
    /// owns the property (ledger row 123). Empty for every store written
    /// since the refusal. Not persisted: a reopen finds the same pairs again
    /// until a snapshot stops re-emitting their declarations.
    dropped_view_indexes: Vec<(String, String)>,
    /// The store has opted in to the V11 format: it has written, or holds, a
    /// V11-only record — a value of one of the five 0.10 types, a unique
    /// constraint or a range index (contract C9).
    ///
    /// Set at open when the snapshot is V11 or the WAL replays such a record,
    /// and on the first write of one, in
    /// [`rewrite_wal_dense_planned`](Self::rewrite_wal_dense_planned), which
    /// writes the V11 snapshot before the record is appended. Never cleared:
    /// dropping the last constraint or range index does not move the store
    /// back, because its WAL and archives still hold V11 records.
    v11_features: bool,
    /// Set while a submission of a group commit is applied without its own
    /// fsync. The V11 upgrade must not run then: its snapshot would hold the
    /// group's earlier submissions before their fsync, and a failed group
    /// fsync would leave them durable after their submitters were told `Err`.
    /// The group upgrades ahead of its first append instead
    /// ([`upgrade_to_v11_for_ops`](Self::upgrade_to_v11_for_ops)).
    in_nosync_group: bool,
    event_sink: Option<Box<dyn Fn(MutationEvent) + Send + Sync>>,
    /// WAL fsync cadence. Default [`FsyncPolicy::Strict`].
    fsync: FsyncPolicy,
    /// Monotonically increasing per-commit counter.  A single `log_then_apply_with`
    /// call increments this once; all events emitted from that call share the same
    /// `commit_seq` value.
    commit_seq: u64,
    /// Commit → wall-clock map, loaded from the `commit_times.bin` sidecar at
    /// open and appended to by `log_then_apply_with` — the one place a commit
    /// is born. Replay does **not** stamp: `apply_frames` re-applies commits
    /// that already happened, and `SystemTime::now()` there would record replay
    /// time as commit time. Empty on a store written before v0.6.11, which
    /// makes every date query answer `NoRecordedTime` rather than guess.
    commit_times: core_storage::commit_times::CommitTimes,
    /// When set, subsequent commits are recorded at this instant instead of the
    /// system clock.
    ///
    /// Sticky on purpose. A backfill replays history that happened over months,
    /// and a day's worth of rows genuinely share one instant — a one-shot flag
    /// would mean setting it before every row of a bulk load, and forgetting one
    /// would stamp that row "now" in the middle of 2026-06. Sticky makes the
    /// failure visible instead: forget to move it and every commit carries the
    /// same timestamp, which a date query answers oddly and an inspection shows
    /// at once.
    commit_time_override: Option<i64>,
    /// `true` only while the open path is replaying, where `load_from_disk`
    /// calls `fulltext.rebuild_all` unconditionally afterwards.
    ///
    /// Replaying an `EnableFulltext` record backfills its pair with a full
    /// `0..ids.len()` scan, and every snapshot re-emits one such record per
    /// enabled pair — so on a snapshotted store the open does that scan once per
    /// pair and then `rebuild_all` clears every posting and does it all again.
    /// The backfill is pure waste *when a rebuild follows*, which is true of the
    /// open path and **false** of `refresh()`: refresh applies peer frames and
    /// then only folds, so its backfill is the only thing that indexes them.
    fulltext_rebuild_follows: bool,
    /// `true` when this handle's full-text index was read from the snapshot
    /// it opened and nothing replayed over it since has spoiled it, so the
    /// open did not build it.
    fulltext_loaded: bool,
    /// `true` when a declaration was applied since the last fold built its
    /// `SchemaListing`, so the next fold builds it again instead of sharing
    /// the previous fold's.
    schema_stale: bool,
    /// Set by the insert arms of `apply` when they index a node that is
    /// already indexed under that field. `add_tokens` appends, so that node's
    /// postings are now doubled: the WAL being replayed holds writes the
    /// snapshot already has — a `keep_wal` snapshot, or a crash between a
    /// snapshot and its WAL replacement. The open rebuilds when it sees this.
    fulltext_reindexed_a_node: bool,
    /// Whether the snapshot this handle opened replaced its WAL when it was
    /// written. One that kept it (`keep_wal`) has the whole WAL replayed over
    /// it at open, and its index is built after that replay, not read.
    base_wal_truncated: bool,
    /// How often that happened during this handle's open. Tests read it.
    fulltext_reindexed_at_open: u64,
    /// `true` only while `load_from_disk` replays the WAL. A node indexed
    /// twice there is a sign the index must be rebuilt, and is left for the
    /// rebuild; anywhere else it is corrected on the spot.
    replaying_at_open: bool,
    /// `true` when `snapshot.intent` may hold something that is no longer
    /// true: this handle found one that did not match, or a snapshot of its
    /// own wrote one and did not get to empty it. Settled, under the write
    /// lock, before the next append.
    intent_unsettled: bool,
    /// Set by [`GraphDb::stop_next_snapshot`]; taken by the next snapshot.
    snapshot_stop: Option<SnapshotStop>,
    /// `true` when `commit_times.bin` was present but would not decode.
    ///
    /// Mirrors `roles: None`: a damaged map must not read as "this store
    /// records no times", because that is also what an honest pre-v0.6.11 store
    /// says. Date queries answer `Corrupt` instead, and nothing is appended to
    /// a file already known to be damaged.
    commit_times_poisoned: bool,
    /// RBAC role definitions loaded from `roles.json` at open.
    ///
    /// `Some(roles)` — loaded successfully (may be empty when no roles are defined).
    /// `None` — `roles.json` was present but corrupt; `mask_for_role` returns
    /// `Err` for any request (fail-loud, never silently grant empty visibility).
    roles: Option<Vec<RoleDef>>,
    /// Memo for [`mask_for_role`](GraphDb::mask_for_role), keyed by
    /// `(role, commit_seq)` — a scoped reader between two writes resolves once.
    ///
    /// Shared by `Arc` with every [`ReaderSnapshot`](crate::reader::ReaderSnapshot)
    /// taken from this handle. Replaced (not cleared) whenever the role
    /// definitions change or the store is reloaded, which `commit_seq` does not
    /// record; see [`RoleMaskCache`](crate::mask::RoleMaskCache).
    role_masks: Arc<crate::mask::RoleMaskCache>,
    /// Which loaded store this handle is, for memos that outlive it.
    ///
    /// `role_masks` needs no such thing — the handle owns it and replaces it —
    /// but a [`Scope`](crate::mask::Scope) is the caller's, so its resolved key
    /// leg is stamped with this alongside `commit_seq`. Minted fresh here and
    /// again in [`reset_for_reload`](GraphDb::reset_for_reload), at exactly the
    /// two points a fresh `RoleMaskCache` is installed; see
    /// [`StoreStamp`](crate::mask::StoreStamp) for the invariant.
    store_id: crate::mask::StoreId,
    /// Live subscriptions.  Entries with a dead `Weak` are pruned on the next
    /// distribute_events call.
    subscriptions: Vec<SubEntry>,
    /// Live query subscriptions. Re-executed on every commit when non-empty.
    /// Dead `Weak` entries are pruned inside `distribute_events`.
    query_subscriptions: Vec<QuerySubEntry>,
    /// Queue capacity for new subscriptions created by this db.  Default is
    /// [`DEFAULT_SUB_CAPACITY`]; can be overridden via [`set_sub_capacity`]
    /// to test Lagged behaviour with small queues.
    sub_capacity: usize,
    /// True for as-of instances opened via [`GraphDb::open_at`].
    /// Every mutation method and `snapshot()` returns [`GraphError::ReadOnly`]
    /// when this flag is set.
    read_only: bool,
    /// Total WAL commit count at the time [`open_at`] was called.
    /// 0 for normal (non-as-of) instances.
    total_wal_commits: u64,
    /// Immutable mmap-backed base snapshot (V8).  When `Some`, `self.topo` is
    /// the WAL-replay overlay (empty at open time, populated by apply()) and
    /// reads go through a merged `TopologyView`.  `self.props` is always
    /// fully materialized (base + WAL replay) for HNSW/IVF and view compat.
    base: Option<Arc<core_storage::v8::MappedBase>>,
    // ── MVCC epoch reader state ───────────────────────────────────────────────
    /// Most-recent full overlay clone.  Initialized at end of `open_with` /
    /// `open_at_with`; refreshed every `FOLD_EVERY_K` commits.
    /// `None` only between struct creation and the first fold.
    fold_overlay: Option<Arc<crate::reader::FrozenOverlay>>,
    /// Whether the engine's provenance began as the snapshot base's PROVENANCE
    /// section (`restore_v8_base`) — so the base can serve as the root of the
    /// reader's derived edges — rather than as a decoded legacy snapshot's.
    engine_from_base: bool,
    /// The derived-edge changes of `delta_tail`, indexed by node.
    tail_derived: Arc<crate::reader::TailDerived>,
    /// Per-commit deltas accumulated since the last fold.
    delta_tail: Vec<Arc<crate::reader::CommitDelta>>,
    /// The materialised effective state shared by every reader snapshot of
    /// the same fold and delta tail, so a snapshot per request does not apply
    /// the tail once per request. See [`crate::reader::SharedEffective`].
    shared_effective: std::sync::Mutex<Option<crate::reader::SharedEffective>>,
    /// How many commits have occurred since the last fold.
    commits_since_fold: usize,
    /// When true, `log_then_apply_with` buffers event notifications instead of
    /// firing them immediately.  Used by the group-commit drain thread to defer
    /// events until after the group fsync (R2: durability before notification).
    /// Cleared to false once the drain thread flushes or discards the buffer.
    defer_events: bool,
    /// Buffered events accumulated while `defer_events` is true.
    deferred_events: Vec<DeferredEvent>,
    /// Set to true by the group-commit drain thread when a group fsync fails
    /// after WAL truncation.  All subsequent mutation attempts return an IO
    /// error until the database is reopened.
    degraded: bool,
    /// Set to `true` after `ensure_v8_base_sections_loaded` has read provenance,
    /// HNSW, and IVF sections from the mmap base into the engine's retained
    /// fields.  `false` on all opens until first use; always `true` for non-V8
    /// opens (base is None, fast-path sets flag immediately).
    v8_sections_loaded: std::sync::atomic::AtomicBool,
    /// Serializes the one-time section population in `ensure_v8_base_sections_loaded`.
    v8_sections_mutex: std::sync::Mutex<()>,
    /// Per-node last-change commit sequence.  `last_change[node_id] = seq` means
    /// the node was last modified by commit `seq`.
    ///
    /// Loaded from V8 section 11 at open; updated on every state-changing commit
    /// and WAL replay frame.  V5-V7 stores start with an empty map; pre-WAL-horizon
    /// nodes return `None` from `last_changed` until they are next mutated.
    ///
    /// See [`Precondition`] for the full touch definition.
    last_change: HashMap<u32, u64>,
    /// WAL archive retention policy set by [`set_wal_archive_retention`].
    /// `None` = unlimited (keep all archives); `Some(N)` = keep N newest archives,
    /// pruning older ones at snapshot time.  0 is treated as unlimited.
    wal_archive_retention: Option<u32>,
    /// Global frame index of the first commit that is still reachable through
    /// surviving archives.  Persisted to `wal.floor` sidecar when pruning occurs.
    /// Default 0 = all history reachable.
    wal_horizon_floor: u64,
    /// True when the surviving archive chain forms a continuous WAL history
    /// starting from the store's first commit (the genesis chain).
    ///
    /// `open_at` may replay archive-resident commits from empty state only when
    /// this flag is true AND `wal_horizon_floor == 0`.  Cleared whenever:
    ///   - a WAL-truncating snapshot (`keep_wal=false`) is taken after archives
    ///     already exist (breaks the chain for subsequent archives), or
    ///   - any archive is pruned (floor advances past zero).
    ///
    /// Persisted via the `wal.genesis` marker file; loaded from it at open.
    archive_genesis_chain: bool,
    /// True when this handle can *prove* the live WAL has never been truncated:
    /// there was no `snapshot.bin` when it opened the store, and it has taken no
    /// truncating snapshot since.
    ///
    /// The archive path's genesis check asks "did a snapshot exist before this
    /// one?" as a proxy for "was the WAL ever truncated". The proxy is sound
    /// across sessions — this binary cannot tell a history-preserving snapshot
    /// from a truncating one once the handle that took it is gone — but inside
    /// one session it is not, and `enable_multiplicity` made that visible: its
    /// forced `keep_wal` snapshot left the WAL entirely intact and yet
    /// permanently disqualified the store from ever receiving a genesis marker
    /// (defect #23). This flag is what the proxy defers to when the answer is
    /// actually known.
    snapshot_preserved_history: bool,
    /// Transient write-authz context set by `write_batch_authz` /
    /// `query_write_authz` for the duration of ONE mutation call.
    /// Always `None` at rest.  Never serialized, never WAL-replayed.
    pending_write_authz: Option<WriteAuthz>,
    /// Slow-query threshold in milliseconds.  0 = disabled.
    /// Seeded from `MUSHROOMDB_SLOW_QUERY_MS` at open; override via
    /// [`GraphDb::set_slow_query_threshold_ms`] (tests must use the setter
    /// — env vars are process-global and race parallel test threads).
    slow_query_threshold_ms: u64,
    /// Ring buffer of recent slow queries (interior-mutable so `query(&self)`
    /// can record entries without requiring `&mut self`).
    slow_queries: Arc<std::sync::Mutex<SlowQueryLog>>,
    /// `(field, label, caller)` triples whose exact-versus-approximate
    /// ambiguity this handle has already explained once. See
    /// [`note_ambiguous_exactness`](GraphDb::note_ambiguous_exactness).
    /// The caller shape is part of the key because the two shapes give
    /// different advice — silencing one with the other would leave a caller
    /// reading advice meant for a signature it does not have.
    /// Advice bookkeeping, not graph state: a reload keeps it, as the
    /// slow-query log does.
    warned_ambiguous_exactness: std::sync::Mutex<HashSet<(String, String, ExactnessCaller)>>,
    /// Instant at which the database was opened (used by `/metrics` uptime).
    started_at: std::time::Instant,
    // ── Multi-process state (cross-process lock + WAL tailing) ────────────────
    /// Byte offset of the WAL prefix already applied to in-memory state.
    ///
    /// Advanced by exactly the encoded length of every frame this handle
    /// appends, and by the decoded byte count of every tail
    /// [`refresh`](GraphDb::refresh) absorbs. Rewound by
    /// [`set_wal_consumed`](GraphDb::set_wal_consumed) when the group-commit
    /// drain thread truncates a failed group. Compared against the WAL's
    /// on-disk length to decide staleness.
    wal_consumed: u64,
    /// The **global 0-based frame index the next appended WAL frame will
    /// occupy** — `wal_horizon_floor` plus every frame currently reachable
    /// through archives and the live WAL.
    ///
    /// This is the space every history surface addresses: `edges_at`,
    /// `was_linked`, both history readouts and `open_at` all index the sequence
    /// [`all_frames`](GraphDb::all_frames) returns, and
    /// [`wal_total_commits`](GraphDb::wal_total_commits) counts it.
    ///
    /// It exists because **a commit is not a frame**. `commit_seq` counts
    /// commits; a commit whose rules fire appends a *second* frame — the
    /// derived-edge history marker — that no counter of commits ever sees. The
    /// two diverge by one frame per rule-firing commit, cumulatively, so
    /// deriving a frame index from `commit_seq` under-reports by more and more
    /// as history grows and resolves every date to an earlier graph. Silently:
    /// an older graph is a plausible answer, not an error.
    ///
    /// Maintained in lockstep with [`wal_consumed`](GraphDb::wal_consumed) —
    /// the same appends advance both, one in frames and one in bytes — so the
    /// two are seeded and rewound at exactly the same places. Keep it that way.
    wal_frames_written: u64,
    /// How much of the live WAL is known fsynced, for readers outside the
    /// engine (the backup worker). Set right after each successful WAL fsync
    /// and when a snapshot replaces `wal.bin`; never read by the engine
    /// itself. Shared, so a [`SharedDb`](crate::SharedDb) reads it without
    /// taking the handle's lock. See [`WalPosition`].
    durable_wal: DurableWal,
    /// Identity of the snapshot this handle's base state came from, as
    /// `(len, mtime_nanos)`. A different value means another process replaced
    /// the snapshot and the WAL no longer continues our state: refresh reloads.
    snapshot_ident: Option<(u64, u64)>,
    /// Identity of the `wal.bin` file this handle's cursor describes
    /// ([`Fs::wal_ident`]). A different file under the name — another
    /// process's snapshot replaced it — makes [`refresh`](GraphDb::refresh)
    /// reload, even when the new file is longer than the cursor.
    wal_ident: Option<u64>,
    /// The options this handle was opened with. Replayed verbatim when
    /// `refresh` has to rebuild from disk.
    open_opts: OpenOptions,
    /// True when this handle holds the cross-process write lock for its whole
    /// lifetime (a plain read-write open). Per-write lock acquisition is a
    /// no-op on such a handle, and never releases the lock.
    holds_lifetime_lock: bool,
    /// True between a failed lock acquisition and the end of the write scope
    /// that failed. Makes every WAL-appending mutation in that scope return
    /// [`GraphError::Busy`] instead of writing.
    lock_denied: bool,
    /// Set with `lock_denied` when the lock attempt failed with an I/O error
    /// rather than because a peer holds it (a refused permission, or any other
    /// fault opening `LOCK`). Makes those mutations report the real cause, never
    /// `Busy`, which is reserved for a lock another process holds.
    lock_failure: Option<GraphError>,
    /// True for an as-of view opened via [`GraphDb::open_at`]. Such a view is
    /// pinned to one commit, so it is never stale and never refreshes — later
    /// commits by any process are deliberately invisible to it.
    pinned: bool,
    /// Correct edge count for the V8 base, computed at open by summing the
    /// out-row lengths of every etype in the base CSR.
    ///
    /// The snapshot header's `edge_count` field is arithmetic carried forward
    /// from the previous snapshot, and is wrong on stores snapshotted while two
    /// rules owned one edge (ledger row 107).  This field replaces it in
    /// `edge_count()` and `stats()` so the reading path is correct even before
    /// the store takes a new snapshot.
    ///
    /// `None` on stores with no V8 base (WAL-only or V5/V7 snapshot opens).
    base_edge_count_from_rows: Option<u64>,
    /// The number of derived-edge retractions the most recent
    /// `log_then_apply_with` call produced, counting only the apply's own
    /// deltas (not the preceding reconcile pass's restorations).
    ///
    /// Reset to 0 before every apply and set after `drain_deltas`.  Used by
    /// `delete_node` to count via-hop derived edges that `provenance_touching`
    /// cannot see (ledger row 50).
    last_apply_retraction_count: u64,
}

/// A handle's WAL cursor and snapshot identity at one moment: what
/// [`GraphDb::is_stale`] compares the files on disk against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiskMark {
    pinned: bool,
    wal_consumed: u64,
    snapshot_ident: Option<(u64, u64)>,
}

impl DiskMark {
    /// Whether the store under `fs` has moved on from this mark: the WAL's
    /// length differs from the cursor, or the snapshot file was replaced.
    /// Metadata only; never true for an as-of view.
    pub(crate) fn is_stale_on<F: Fs>(&self, fs: &F) -> Result<bool> {
        if self.pinned {
            return Ok(false);
        }
        if fs.wal_len().map_err(GraphError::Io)? != self.wal_consumed {
            return Ok(true);
        }
        Ok(fs.snapshot_ident().map_err(GraphError::Io)? != self.snapshot_ident)
    }

    /// [`Self::is_stale_on`], against files already looked at.
    pub(crate) fn is_stale_at(&self, disk: &DiskState) -> bool {
        !self.pinned
            && (disk.wal_len != self.wal_consumed || disk.snapshot_ident != self.snapshot_ident)
    }
}

/// What a staleness check reads off the store's files: the WAL's length and
/// the snapshot file's identity. Metadata only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiskState {
    wal_len: u64,
    snapshot_ident: Option<(u64, u64)>,
}

impl DiskState {
    /// Look at `fs`'s files, the WAL first, as [`DiskMark::is_stale_on`] does.
    pub(crate) fn read<F: Fs>(fs: &F) -> Result<Self> {
        Ok(Self {
            wal_len: fs.wal_len().map_err(GraphError::Io)?,
            snapshot_ident: fs.snapshot_ident().map_err(GraphError::Io)?,
        })
    }
}

/// One group of deferred event notifications, held until the group fsync
/// completes.  Replayed by [`GraphDb::flush_deferred_events`].
struct DeferredEvent {
    rec: core_storage::WalRecord,
    engine_deltas: Vec<EngineEdgeDelta>,
    seq: u64,
    ingest: Option<(String, usize)>,
}

/// Options for [`GraphDb::open_with_options`].
#[derive(Clone, Copy, Debug)]
pub struct OpenOptions {
    /// Rewrite an old-format snapshot to the current VERSION after a
    /// successful load (default `true`). The old snapshot is kept as
    /// `snapshot.bin.bak` until the next clean open at the current version,
    /// at which point the `.bak` is deleted.
    ///
    /// Set to `false` to open a store without touching any on-disk files
    /// (useful for read-only inspection of a store at an older format).
    pub auto_migrate: bool,

    /// Write the valid WAL prefix back over a torn tail on open (default
    /// `true`). Truncating a genuinely torn tail is correct crash recovery.
    ///
    /// Set to `false` for an unattended reader. The valid prefix is still
    /// decoded and replayed in memory, but nothing is written: a reader that
    /// opens while another process is mid-append would otherwise discard a
    /// frame that writer believes durable. `mushroomdb recall`, which runs on
    /// every prompt, passes `false` for exactly this reason.
    pub repair_wal: bool,

    /// Open without ever writing to the store (default `false`).
    ///
    /// A read-only handle:
    /// - returns [`GraphError::ReadOnly`] from every mutation and from
    ///   `snapshot()`;
    /// - performs no disk write at open — no WAL repair write-back and no
    ///   auto-migration rewrite, whatever the other two flags say;
    /// - never takes the cross-process write lock, so it opens immediately even
    ///   while another process is writing, and never makes a writer wait.
    ///
    /// [`refresh`](GraphDb::refresh) and [`is_stale`](GraphDb::is_stale) work
    /// normally, so a read-only handle can follow another process's commits.
    pub read_only: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            auto_migrate: true,
            repair_wal: true,
            read_only: false,
        }
    }
}

/// How long a writer polls for the cross-process write lock before giving up
/// with [`GraphError::Busy`].
///
/// Long enough to ride out another process's commit (a batch apply plus one
/// fsync), short enough that a stuck peer surfaces as an error rather than a
/// hang.
pub const WRITE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Refusal when a `MERGE` create cannot choose a namespace.
///
/// A role bound to two or more namespaces cannot have its create arm land in
/// `default`, and the statement did not name `ns`. The role must name one.
pub const MERGE_CREATE_NEEDS_ONE_NAMESPACE: &str =
    "role-bound token: MERGE create requires the role to name one namespace";

/// Interval between poll attempts while waiting for the cross-process lock.
pub(crate) const LOCK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Why `load_from_disk` is running, which decides whether it may repair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadOrigin {
    /// A fresh open. Crash recovery is this handle's job: a torn WAL tail is
    /// the signature of a crash and truncating it is correct, and archives
    /// orphaned by an interrupted prune can be swept.
    Open,
    /// A reload driven by [`GraphDb::refresh`], because another process
    /// replaced the snapshot. Nothing here is crash recovery — the store is
    /// live and someone else is writing it — so this origin writes nothing.
    Reload,
}

/// Authorization context carried by `write_batch_authz` / `query_write_authz`.
///
/// `None` at the call site = full authority (today's zero-cost behavior).
/// `Some(WriteAuthz)` = role-scoped: the decision table (plan §"authz decision
/// table") is evaluated per-op inside `commit_logged_batch` BEFORE any WAL
/// record is built.  A denial returns an error with no WAL frame written.
///
/// The mask is ALWAYS `Omit`-mode: role-token paths must never acknowledge
/// hidden-node existence to callers.
#[derive(Clone, Debug)]
pub struct WriteAuthz {
    pub role: String,
    pub scope: WriteScope,
    /// Resolved by `mask_for_role` under the same write guard as the mutation.
    /// Always `Omit`-mode — never `Stub`.
    pub mask: crate::mask::NodeMask,
}

/// The error every role surface gives when `roles.json` did not parse at open.
///
/// One text, so `mask_for_role` and [`GraphDb::roles_checked`] cannot drift
/// apart on the same cause.
fn roles_poisoned() -> GraphError {
    GraphError::Corrupt {
        detail: "roles.json was corrupt at open; fix the file and re-open to restore role access"
            .into(),
    }
}

/// Write `bytes` to `snapshot.bin.bak` atomically with full fsync.
///
/// Uses [`RealFs::write_atomic`] which applies `F_FULLFSYNC` on macOS and
/// `sync_all` on other platforms, then renames the `.tmp` file into place and
/// syncs the directory entry. This is the only correct path for writing the
/// `.bak` — plain `std::fs::write + sync_all` misses both `F_FULLFSYNC` and
/// the directory sync.
pub fn write_snapshot_bak(dir: &std::path::Path, bytes: &[u8]) -> crate::Result<()> {
    use core_storage::fs::{FileId, Fs as _};
    RealFs::new(dir)
        .map_err(core_storage::GraphError::Io)?
        .write_atomic(FileId::SnapshotBak, bytes)
        .map_err(core_storage::GraphError::Io)
}

/// Return the on-disk snapshot format version without decoding the full snapshot.
///
/// Reads only the 6-byte header (magic + version LE). Returns `None` when no
/// snapshot file exists (WAL-only store). Returns an error if the header is
/// malformed.
pub fn snapshot_version_at(dir: &std::path::Path) -> crate::Result<Option<u16>> {
    use std::io::Read as _;
    let path = dir.join("snapshot.bin");
    let mut header = [0u8; 6];
    let n = match std::fs::File::open(&path) {
        Ok(mut f) => f.read(&mut header).map_err(core_storage::GraphError::Io)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(core_storage::GraphError::Io(e)),
    };
    core_storage::snapshot::peek_version(&header[..n])
}

/// Where [`GraphDb::stop_next_snapshot`] makes the next snapshot fail, leaving
/// the files as a crash at that point would. For tests of what an interrupted
/// snapshot leaves behind; not part of the stable surface.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotStop {
    /// The intent is written; `snapshot.bin` is still the old one.
    BeforeSnapshotWrite,
    /// `snapshot.bin` is the new one; nothing else has moved.
    AfterSnapshotWrite,
    /// As above, and the history floor has been advanced; the WAL is still
    /// the one the snapshot was made from.
    BeforeWalReplacement,
    /// The WAL is replaced; the intent has not been emptied.
    AfterWalReplacement,
}

/// Options for [`GraphDb::snapshot_with`].
#[derive(Debug, Clone, Default)]
pub struct SnapshotOptions {
    /// When `true`, the WAL is preserved after the snapshot write.
    /// Pre-snapshot commits remain reachable via [`GraphDb::open_at`].
    /// When `false` (the default), the WAL is truncated to a minimal
    /// baseline so cold-start replay stays fast.
    pub keep_wal: bool,
    /// When `true`, the current WAL is renamed to `wal.<commit_seq>.archive`
    /// before a fresh WAL baseline is written (history-preserving snapshot).
    ///
    /// This is the feature opt-in: `false` (the default) leaves the existing
    /// truncation / keep-wal behaviour byte-identical.  `archive_wal` takes
    /// precedence over `keep_wal` when both are set.
    ///
    /// Archives can be scanned by [`GraphDb::node_history`],
    /// [`GraphDb::edge_history`], [`GraphDb::was_linked`], and
    /// [`GraphDb::open_at`], extending the reachable history horizon across
    /// snapshot boundaries.
    pub archive_wal: bool,
}

/// Derive the scan-label sym for the commit-skip fast-path.
///
/// Walks `ops` to find the plan's leading scan op (`ScanLabel`, `IndexScan`,
/// `IndexIntersect` or `IndexRangeScan`) with a concrete label string, then
/// interns it.
///
/// Returns `None` in all cases where skipping is unsafe:
/// - Any `Expand` op is present (edge traversal; edges change results regardless
///   of node labels).
/// - The leading scan has no label (`ScanLabel { label: None }` — full scan).
/// - No recognizable leading scan op is found.
///
/// This is the conservative v0.4.3 boundary. The caller stores the result in
/// [`QuerySubEntry::scan_label`] at subscribe time; `None` means always execute.
fn extract_scan_label(ops: &[PlanOp], syms: &mut Interner) -> Option<u32> {
    // Any Expand → must always re-execute (edges can change join results).
    if ops.iter().any(|op| matches!(op, PlanOp::Expand { .. })) {
        return None;
    }
    for op in ops {
        match op {
            PlanOp::ScanLabel {
                label: Some(label), ..
            } => return Some(syms.intern(label)),
            PlanOp::IndexScan {
                label: Some(label), ..
            } => return Some(syms.intern(label)),
            PlanOp::IndexIntersect {
                label: Some(label), ..
            } => return Some(syms.intern(label)),
            PlanOp::IndexRangeScan { label, .. } => return Some(syms.intern(label)),
            _ => {}
        }
    }
    None
}

/// How an as-of read is restricted — the argument to
/// [`GraphDb::query_at_scoped`].
///
/// Every variant is resolved against the graph **as it was at the requested
/// commit**, not against the current graph.
#[derive(Debug, Clone, Copy)]
pub enum AsOfScope<'a> {
    /// Everything the named role may see. The role *definition* is the current
    /// one — `roles.json` is a sidecar and has no past version — but its
    /// `keys` and `labels` are resolved against the as-of graph.
    Role(&'a str),
    /// An explicit node-key allow-list. Keys that did not exist at that commit
    /// resolve to nothing.
    Keys(&'a [String]),
    /// A role intersected with a client-supplied allow-list. The intersection
    /// is the never-widen rule: a client mask can only narrow a role.
    RoleAndKeys(&'a str, &'a [String]),
    /// Every live node in one namespace, as the graph was at that commit.
    ///
    /// A namespace cannot change — it is set at insert and immutable — so the
    /// answer is simply "the nodes that existed then and are in this
    /// namespace". A name no node uses resolves to nothing, never to
    /// everything.
    Namespace(&'a str),
}

impl GraphDb<RealFs> {
    /// Open the database at `dir` with default options.
    ///
    /// Equivalent to `open_with_options(dir, OpenOptions::default())`.
    /// Old-format snapshots (V5, V6) are automatically migrated to the
    /// current version on a successful load (see [`OpenOptions::auto_migrate`]).
    pub fn open(dir: &std::path::Path) -> Result<Self> {
        Self::open_with_options(dir, OpenOptions::default())
    }

    /// Open the database at `dir` with explicit options.
    ///
    /// When `opts.auto_migrate` is `true` (the default) and the on-disk
    /// snapshot is an older format version, this function:
    ///   1. Copies the current `snapshot.bin` to `snapshot.bin.bak` (atomic
    ///      + fsynced) before any modification.
    ///   2. Rewrites `snapshot.bin` at the current format version via
    ///      [`GraphDb::snapshot_with`] with `keep_wal: true` (WAL preserved).
    ///
    /// If migration fails the error is returned and the original files are
    /// intact (the `.bak` was written before the new snapshot was attempted).
    ///
    /// A clean open that finds the snapshot already at the current version
    /// deletes any leftover `.bak` file.
    ///
    /// WAL-only stores (no snapshot) are never auto-migrated on open.
    ///
    /// `opts.repair_wal` controls the other write this function can make; see
    /// [`OpenOptions::repair_wal`]. With both flags `false` the open touches
    /// no file on disk.
    pub fn open_with_options(dir: &std::path::Path, opts: OpenOptions) -> Result<Self> {
        Self::open_dir(dir, opts, true)
    }

    /// Open without taking the cross-process write lock for the handle's
    /// lifetime.
    ///
    /// Only [`SharedDb`](crate::SharedDb) uses this: a long-lived server holds
    /// its handle open indefinitely, so it takes the lock per write instead of
    /// keeping every other process out of the store for as long as it runs.
    pub(crate) fn open_unlocked(dir: &std::path::Path) -> Result<Self> {
        Self::open_dir(dir, OpenOptions::default(), false)
    }

    fn open_dir(dir: &std::path::Path, opts: OpenOptions, hold_lock: bool) -> Result<Self> {
        crate::restore::refuse_half_installed(dir)?;
        // Header-only peek — 6 bytes, no full decode.
        let snap_version = snapshot_version_at(dir)?;

        // Full load: decode snapshot + replay WAL + rebuild indexes.
        let mut db = Self::open_generic(RealFs::new(dir)?, opts, hold_lock)?;

        // A read-only handle writes nothing at open, so it never migrates —
        // the old-format snapshot is loaded and left exactly as it is.
        if opts.auto_migrate && !opts.read_only {
            match snap_version {
                Some(ver) if ver < core_storage::snapshot::VERSION => {
                    let _tm = std::time::Instant::now();
                    // Copy the original snapshot to .bak at OS level — no in-memory
                    // buffer required for a 2+ GiB file.
                    //
                    // Crash-safety: snapshot.bin remains intact (write_atomic inside
                    // snapshot_with uses a .tmp+rename) until the V8 write succeeds.
                    // A torn .bak on crash is acceptable because the original
                    // snapshot.bin is the authoritative source until after the rename.
                    std::fs::copy(dir.join("snapshot.bin"), dir.join("snapshot.bin.bak"))
                        .map_err(core_storage::GraphError::Io)?;
                    trace_migrate!("bak copy done", _tm);
                    // Rewrite snapshot at current version; keep WAL intact.
                    db.snapshot_with(SnapshotOptions {
                        keep_wal: true,
                        ..SnapshotOptions::default()
                    })?;
                    trace_migrate!("snapshot_with done", _tm);
                }
                Some(_) => {
                    // Already current version: remove any leftover .bak.
                    let bak = dir.join("snapshot.bin.bak");
                    if bak.exists() {
                        std::fs::remove_file(&bak).map_err(core_storage::GraphError::Io)?;
                    }
                }
                None => {
                    // WAL-only store — nothing to migrate on open.
                }
            }
        }

        Ok(db)
    }

    /// Open a read-only view of the database as it existed after `commit`.
    ///
    /// Commit indices are 0-based over the current WAL: commit 0 is the state
    /// after the first WAL frame, commit N-1 is the state after the N-th (most
    /// recent) frame.  Call [`GraphDb::open`] to read the full current state.
    ///
    /// **Replay base.** [`GraphDb::snapshot`] truncates the WAL when it runs,
    /// so as-of can only reach commits recorded in the current WAL (those
    /// written after the most recent snapshot, or all commits if no snapshot
    /// was ever taken).  Commit 0 in `open_at` always refers to the first
    /// frame in the WAL that exists on disk, not the first ever write to the
    /// database.  When the on-disk snapshot recorded that it truncated the
    /// WAL (V7, default `keep_wal: false`), it is loaded as the base state
    /// before frame replay, so the as-of view includes all pre-snapshot data.
    /// Snapshots written with `keep_wal: true` (and legacy V5/V6 snapshots)
    /// are ignored and replay is WAL-only, as before.
    ///
    /// **Read-only.** Every mutation method and `snapshot()` on the returned
    /// instance returns [`GraphError::ReadOnly`].  Queries, `explain()`, and
    /// `stats()` work normally.
    ///
    /// # Errors
    /// - [`GraphError::CommitOutOfRange`] if `commit >= wal_commit_count` (including
    ///   when the WAL is empty after a snapshot).
    pub fn open_at(dir: &std::path::Path, commit: u64) -> Result<Self> {
        crate::restore::refuse_half_installed(dir)?;
        Self::open_at_with(RealFs::new(dir)?, commit)
    }

    /// Run a **read-only** Cypher query against the graph as it existed at
    /// `commit` — the "time-travel" / agent-replay query. Opens a temporal view
    /// of this store's directory at that commit and executes the read there.
    ///
    /// The current instance is unaffected. Write statements are rejected (the
    /// temporal view is read-only). `commit` is a 0-based WAL commit index;
    /// `commit == wal_commit_count` (or `open_at`'s range) yields the newest
    /// state. Prefer this over holding many historical instances open.
    ///
    /// # Errors
    /// - [`GraphError::CommitOutOfRange`] if `commit` is past the WAL horizon.
    /// - A query error for a malformed or write query.
    pub fn query_at(
        &self,
        commit: u64,
        cypher: &str,
        params: &std::collections::BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let temporal = self.open_at_for_read(commit, cypher)?;
        temporal.query(cypher, params)
    }

    /// Run a **read-only** Cypher query at `commit`, restricted by `scope`.
    ///
    /// The **graph** is as of `commit`; the **role definition** is as it is
    /// now, because `roles.json` is a sidecar and is never a WAL record — it
    /// has no past version to read. A role's `keys` and `labels` are resolved
    /// against the commit-`commit` graph, so a role that may see a label sees
    /// exactly the nodes that carried it then, and an explicit key that did
    /// not exist yet resolves to nothing.
    ///
    /// [`AsOfScope::RoleAndKeys`] intersects the two: a client allow-list can
    /// only narrow what a role may see, never widen it.
    ///
    /// Write statements are rejected, exactly as [`GraphDb::query_at`] rejects
    /// them.
    ///
    /// # Errors
    /// - [`GraphError::CommitOutOfRange`] if `commit` is outside the retained
    ///   range; the error carries that range.
    /// - [`GraphError::KeyNotFound`] with a `role:` prefix for an unknown role,
    ///   or [`GraphError::Corrupt`] when `roles.json` was corrupt at open.
    /// - A query error for a malformed or write query.
    pub fn query_at_scoped(
        &self,
        commit: u64,
        cypher: &str,
        params: &std::collections::BTreeMap<String, Value>,
        scope: AsOfScope<'_>,
    ) -> Result<ResultSet> {
        let temporal = self.open_at_for_read(commit, cypher)?;
        let mask = temporal.mask_at_scope(scope)?;
        temporal.query_masked(cypher, params, &mask)
    }

    /// As [`GraphDb::query_at_scoped`], with `namespace` intersected into
    /// whatever `scope` resolves to.
    ///
    /// This is what a surface needs when a caller passes `namespace` beside a
    /// `role` or a client mask on a time-travel read: [`AsOfScope`] names one
    /// restriction, and the namespace is a second one that composes with it
    /// rather than replacing it. The intersection is the never-widen rule — a
    /// namespace can only narrow what the scope already allows — and both legs
    /// are resolved against the graph as it was at `commit`.
    ///
    /// `AsOfScope::Namespace(ns)` is still the way to ask for a namespace alone.
    pub fn query_at_scoped_in_namespace(
        &self,
        commit: u64,
        cypher: &str,
        params: &std::collections::BTreeMap<String, Value>,
        scope: AsOfScope<'_>,
        namespace: &str,
    ) -> Result<ResultSet> {
        let temporal = self.open_at_for_read(commit, cypher)?;
        let mask = temporal
            .mask_at_scope(scope)?
            .intersect(&temporal.mask_for_namespace(namespace));
        temporal.query_masked(cypher, params, &mask)
    }

    /// Run a **read-only** Cypher query at `commit`, restricted by a
    /// [`Scope`](crate::mask::Scope).
    ///
    /// [`AsOfScope`] names *one* restriction — a role, a key list, a namespace,
    /// or a role-and-keys pair. A `Scope` is the general shape a handle carries,
    /// and nesting can give it several role or namespace legs at once, so it
    /// cannot be spelled as an `AsOfScope`. This is the entry point a scoped
    /// handle uses for time travel; `query_at_scoped` stays the way to ask for
    /// one named restriction.
    ///
    /// Both the graph and the scope's key and namespace legs are resolved
    /// against `commit`; a role's *definition* is the current one, because
    /// `roles.json` is a sidecar with no past version — the same split
    /// [`GraphDb::query_at_scoped`] documents.
    ///
    /// The scope resolves **cold** here: a temporal handle is its own store, so
    /// its ids could never be served to a live read, but filling the scope's
    /// one-entry key memo from a handle thrown away at the end of this call
    /// would evict the live entry for nothing. See
    /// [`Scope::resolve_uncached`](crate::mask::Scope::resolve_uncached).
    ///
    /// # Errors
    /// - [`GraphError::CommitOutOfRange`] if `commit` is outside the retained
    ///   range; the error carries that range.
    /// - [`GraphError::KeyNotFound`] with a `role:` prefix for an unknown role,
    ///   or [`GraphError::Corrupt`] when `roles.json` was corrupt at open.
    /// - A query error for a malformed or write query.
    pub fn query_at_with_scope(
        &self,
        commit: u64,
        cypher: &str,
        params: &std::collections::BTreeMap<String, Value>,
        scope: &crate::mask::Scope,
    ) -> Result<ResultSet> {
        let temporal = self.open_at_for_read(commit, cypher)?;
        let mask = scope.resolve_uncached(&temporal)?;
        temporal.query_masked(cypher, params, &mask)
    }

    /// Open the temporal view for a time-travel read and refuse write Cypher.
    ///
    /// Shared by [`GraphDb::query_at`] and [`GraphDb::query_at_scoped`] so both
    /// resolve the commit and reject writes identically.
    fn open_at_for_read(&self, commit: u64, cypher: &str) -> Result<Self> {
        let dir = self.fs.dir().to_path_buf();
        let temporal = Self::open_at(&dir, commit)?;
        if is_write_tokens(&lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?) {
            return Err(GraphError::QueryError {
                detail: "query_at is read-only: write statements are not permitted in a \
                         time-travel query"
                    .into(),
            });
        }
        Ok(temporal)
    }
}

/// How many conflicting nodes `GraphError::ConstraintCreationConflict` lists.
const CONSTRAINT_CONFLICTS_LISTED: usize = 100;

/// Where two values the scan can find equal always share a bucket: numbers by
/// the bit pattern of their `f64` (so `Int` 2, `Float` 2.0, and both zeros
/// meet), every other indexable value by its index key. `None` for a value
/// that equals nothing a constraint could hold — `NaN`, or one with no index
/// key (list, map, duration).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum EqualityBucket {
    Number(u64),
    Key(ValueKey),
}

///
/// The create-time scan over existing data
/// (`GraphDb::existing_unique_violations`) groups values with this and
/// compares each bucket with `values_equal`, never by `ValueKey`: a
/// `ValueKey` keeps `Int` 2 and `Float` 2.0 apart, so a scan keyed on it would
/// accept a store the write path then refuses.
pub(crate) fn equality_bucket(value: &Value) -> Option<EqualityBucket> {
    let number = |f: f64| {
        if f.is_nan() {
            None
        } else if f == 0.0 {
            Some(EqualityBucket::Number(0.0_f64.to_bits()))
        } else {
            Some(EqualityBucket::Number(f.to_bits()))
        }
    };
    match value {
        Value::Int(i) => number(*i as f64),
        Value::Float(f) => number(*f),
        other => ValueKey::from_value(other).map(EqualityBucket::Key),
    }
}

impl<F: Fs> GraphDb<F> {
    /// Open over an arbitrary [`Fs`], repairing a torn WAL tail as usual.
    pub fn open_with(fs: F) -> Result<Self> {
        Self::open_with_repair(fs, true)
    }

    /// As [`GraphDb::open_with`], but `repair_wal: false` decodes the valid WAL
    /// prefix without writing the truncation back. See
    /// [`OpenOptions::repair_wal`].
    pub fn open_with_repair(fs: F, repair_wal: bool) -> Result<Self> {
        Self::open_generic(
            fs,
            OpenOptions {
                repair_wal,
                ..OpenOptions::default()
            },
            true,
        )
    }

    /// Shared open path.
    ///
    /// `hold_lock` requests the cross-process write lock for the whole handle
    /// lifetime — the right behaviour for a plain read-write `GraphDb`, whose
    /// owner writes through it directly. [`SharedDb`](crate::SharedDb) passes
    /// `false` and takes the lock per write instead, so that a long-lived
    /// server does not keep every other process out of the store.
    ///
    /// A read-only open never takes the lock regardless of `hold_lock`.
    fn open_generic(fs: F, opts: OpenOptions, hold_lock: bool) -> Result<Self> {
        let mut db = Self::new_empty(fs, opts);
        db.read_only = opts.read_only;
        if hold_lock && !opts.read_only {
            if !db.poll_lock(WRITE_LOCK_WAIT)? {
                return Err(GraphError::Busy { holder: None });
            }
            db.holds_lifetime_lock = true;
        }
        db.load_from_disk(LoadOrigin::Open)?;
        Ok(db)
    }

    /// A handle with no state loaded: every field at its empty value, the
    /// filesystem and options in place. Only [`load_from_disk`] makes it
    /// usable.
    fn new_empty(fs: F, opts: OpenOptions) -> Self {
        Self {
            fs,
            ids: Arc::new(IdMap::new()),
            syms: Arc::new(Interner::new()),
            topo: Arc::new(Topology::new()),
            props: Arc::new(ColumnStore::new()),
            labels: Arc::new(Vec::new()),
            ns_names: vec![NS_DEFAULT.to_string()],
            node_ns: Vec::new(),
            edge_props: Arc::new(EdgeProps::new()),
            engine: RuleEngine::new(),
            view_store: ViewStore::new(),
            fulltext: Arc::new(FulltextIndex::new()),
            prop_index: Arc::new(PropertyIndex::new()),
            multiplicity: false,
            constraints: Vec::new(),
            dropped_view_indexes: Vec::new(),
            v11_features: false,
            in_nosync_group: false,
            event_sink: None,
            fsync: FsyncPolicy::Strict,
            commit_seq: 0,
            commit_times: core_storage::commit_times::CommitTimes::default(),
            commit_times_poisoned: false,
            fulltext_rebuild_follows: false,
            fulltext_loaded: false,
            schema_stale: true,
            fulltext_reindexed_a_node: false,
            base_wal_truncated: false,
            fulltext_reindexed_at_open: 0,
            replaying_at_open: false,
            intent_unsettled: false,
            snapshot_stop: None,
            commit_time_override: None,
            roles: Some(vec![]),
            role_masks: Arc::new(crate::mask::RoleMaskCache::new()),
            store_id: crate::mask::StoreId::next(),
            subscriptions: Vec::new(),
            query_subscriptions: Vec::new(),
            sub_capacity: DEFAULT_SUB_CAPACITY,
            read_only: false,
            total_wal_commits: 0,
            base: None,
            fold_overlay: None,
            engine_from_base: false,
            tail_derived: Arc::default(),
            delta_tail: Vec::new(),
            shared_effective: std::sync::Mutex::new(None),
            commits_since_fold: 0,
            defer_events: false,
            deferred_events: Vec::new(),
            degraded: false,
            v8_sections_loaded: std::sync::atomic::AtomicBool::new(false),
            v8_sections_mutex: std::sync::Mutex::new(()),
            last_change: HashMap::new(),
            wal_archive_retention: None,
            wal_horizon_floor: 0,
            archive_genesis_chain: false,
            // Nothing is proven until `load_from_disk` has looked at the store.
            snapshot_preserved_history: false,
            pending_write_authz: None,
            slow_query_threshold_ms: std::env::var("MUSHROOMDB_SLOW_QUERY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100),
            slow_queries: Arc::new(std::sync::Mutex::new(SlowQueryLog {
                entries: std::collections::VecDeque::new(),
                total: 0,
            })),
            warned_ambiguous_exactness: std::sync::Mutex::new(HashSet::new()),
            started_at: std::time::Instant::now(),
            wal_consumed: 0,
            wal_frames_written: 0,
            durable_wal: DurableWal::default(),
            snapshot_ident: None,
            wal_ident: None,
            open_opts: opts,
            holds_lifetime_lock: false,
            lock_denied: false,
            lock_failure: None,
            pinned: false,
            base_edge_count_from_rows: None,
            last_apply_retraction_count: 0,
        }
    }

    /// Return every field describing stored graph state to its empty value,
    /// leaving this handle's own identity alone.
    ///
    /// Preserved on purpose: the filesystem, open options, lock ownership, the
    /// event sink and subscriptions, fsync policy, degraded flag, and the
    /// slow-query configuration and log. A caller that registered a sink or a
    /// subscription keeps it across a reload.
    fn reset_for_reload(&mut self) {
        self.ids = Arc::new(IdMap::new());
        self.syms = Arc::new(Interner::new());
        self.topo = Arc::new(Topology::new());
        self.props = Arc::new(ColumnStore::new());
        self.labels = Arc::new(Vec::new());
        self.ns_names = vec![NS_DEFAULT.to_string()];
        self.node_ns = Vec::new();
        self.edge_props = Arc::new(EdgeProps::new());
        self.engine = RuleEngine::new();
        self.engine_from_base = false;
        self.view_store = ViewStore::new();
        self.fulltext = Arc::new(FulltextIndex::new());
        self.prop_index = Arc::new(PropertyIndex::new());
        self.schema_stale = true;
        // Cleared like every other declaration: a reload replays the store's own
        // WAL, and the opt-in comes back from it or not at all.
        self.multiplicity = false;
        // Like the multiplicity opt-in: the snapshot and the WAL say it again.
        self.v11_features = false;
        self.constraints = Vec::new();
        self.dropped_view_indexes = Vec::new();
        self.commit_seq = 0;
        self.commit_times = core_storage::commit_times::CommitTimes::default();
        self.commit_times_poisoned = false;
        self.fulltext_rebuild_follows = false;
        self.fulltext_loaded = false;
        self.fulltext_reindexed_a_node = false;
        self.base_wal_truncated = false;
        self.fulltext_reindexed_at_open = 0;
        self.replaying_at_open = false;
        self.commit_time_override = None;
        self.roles = Some(vec![]);
        // A fresh cache, not a cleared one: any reader snapshot still holding
        // the old `Arc` keeps it to itself, so nothing it memoised against the
        // pre-reload store can be read back through this handle.
        self.role_masks = Arc::new(crate::mask::RoleMaskCache::new());
        // The same move for memos this handle does not own. `commit_seq` is
        // zeroed just above and reseeded from `max(last_change)`, which a
        // delete-only commit leaves where it was — so a reload can land back on
        // a sequence a caller's `Scope` already cached a mask at. A new id is
        // what makes that entry stop matching.
        self.store_id = crate::mask::StoreId::next();
        self.total_wal_commits = 0;
        self.base = None;
        self.fold_overlay = None;
        self.delta_tail = Vec::new();
        self.tail_derived = Arc::default();
        self.commits_since_fold = 0;
        self.deferred_events = Vec::new();
        self.v8_sections_loaded
            .store(false, std::sync::atomic::Ordering::Release);
        self.last_change = HashMap::new();
        self.wal_horizon_floor = 0;
        self.archive_genesis_chain = false;
        // Re-derived by `load_from_disk` from the store it is about to read.
        self.snapshot_preserved_history = false;
        self.pending_write_authz = None;
        self.wal_consumed = 0;
        self.wal_frames_written = 0;
        self.snapshot_ident = None;
        self.wal_ident = None;
        self.base_edge_count_from_rows = None;
    }

    /// Load the snapshot base and replay the WAL into an empty handle — the
    /// whole of what opening a store does after the struct exists.
    ///
    /// Split out of the open path so that [`refresh`](GraphDb::refresh) can
    /// rebuild a handle in place, without ownership of `F`, when another
    /// process replaces the snapshot underneath it.
    ///
    /// `origin` decides whether the two repair writes this function can make
    /// are appropriate; see [`LoadOrigin`].
    fn load_from_disk(&mut self, origin: LoadOrigin) -> Result<usize> {
        // Both writes below are crash recovery, and only an open is entitled to
        // perform them. A read-only handle promises to touch nothing, and a
        // reload driven by `refresh` is looking at a store another process is
        // actively writing: what looks like a torn tail there is a peer
        // mid-append, and what looks like an orphaned archive may be one that
        // peer is about to reference.
        let may_repair = origin == LoadOrigin::Open && !self.open_opts.read_only;
        let repair_wal = self.open_opts.repair_wal && may_repair;
        let db = self;
        db.wal_horizon_floor = db.fs.read_horizon_floor()?;
        db.archive_genesis_chain = db.fs.has_genesis_marker();
        // Opening cleanup: remove orphaned archives — archives whose frames all
        // fall below the horizon floor.  Orphans arise when a crash interrupted
        // the retention-prune sequence after the floor was written but before
        // all surplus archives were deleted.  Safe to delete: floor already
        // accounts for their frames.
        if may_repair {
            db.cleanup_orphaned_archives()?;
        }
        let _t0 = std::time::Instant::now();
        // Peek 6 bytes to determine snapshot version without reading the full
        // file. For RealFs this is a true partial read (O(1)); for SimFs the
        // default impl reads all bytes and truncates (still correct).
        let snap_header = db.fs.read_prefix(FileId::Snapshot, 6)?;
        // V8, V9 and V10 share the mmap-able container; V9 only adds section 12
        // and V10 adds nothing but its version stamp. A version outside that set
        // falls through to the full-read path below, where `snapshot::decode`
        // either handles it (V5–V7) or refuses it by name — which is what stops
        // an older binary before it reaches the WAL.
        let is_v8 = snap_header.len() >= 6
            && &snap_header[0..4] == b"GDB1"
            && core_storage::snapshot::is_mmap_container(u16::from_le_bytes([
                snap_header[4],
                snap_header[5],
            ]));
        // The version this store is stamped with, or `None` when it has never
        // been snapshotted. Read from the same six bytes, with no second read.
        let snapshot_version = if snap_header.len() >= 6 && &snap_header[0..4] == b"GDB1" {
            Some(u16::from_le_bytes([snap_header[4], snap_header[5]]))
        } else {
            None
        };
        // A V11 snapshot is the store's own word that it has opted in (C9);
        // the replay below finds the same from any V11-only record it meets.
        if snapshot_version.is_some_and(|v| v >= core_storage::snapshot::VERSION_11) {
            db.v11_features = true;
        }
        // The declarations a V11 snapshot carries itself (section 18), applied
        // at the `held` boundary of the replay below — the point of the WAL
        // the snapshot was taken at. `None` for every earlier snapshot.
        let mut snapshot_declarations = None;
        // No snapshot means no snapshot has ever truncated the WAL, so this
        // handle can prove the history is whole. Once a snapshot exists that
        // this handle did not take, it cannot: see `snapshot_preserved_history`.
        db.snapshot_preserved_history = snap_header.is_empty();
        if is_v8 {
            // V8: map the file zero-copy (RealFs) or read full bytes (SimFs).
            // No 2.4GB heap Vec is allocated on RealFs.
            let mapped = Arc::new(
                if let Some(snap_path) = db.fs.snapshot_path() {
                    core_storage::v8::MappedBase::map(&snap_path)
                } else {
                    let snap_bytes = db.fs.read(FileId::Snapshot)?;
                    core_storage::v8::MappedBase::from_bytes(snap_bytes)
                }
                .map_err(|e| GraphError::Corrupt {
                    detail: format!("v8: mmap open: {e:?}"),
                })?,
            );
            db.restore_v8_base(Arc::clone(&mapped))?;
            trace_open!("restore_v8_base", _t0);
            db.base = Some(mapped);
            trace_open!("base assigned", _t0);
            // Only a snapshot from a writer that could leave a deleted node's
            // edges behind is searched for them.
            if !db.base.as_ref().is_some_and(|base| base.edges_swept()) {
                db.drop_edges_of_retired_nodes();
            }
            // Only a snapshot whose state has not had the lost-provenance
            // repair is repaired at the first write (ledger rows 111, 113).
            if db.base.as_ref().is_some_and(|base| base.prov_reconciled()) {
                db.engine.mark_lost_provenance_reconciled();
            }
            db.load_fulltext_from_base();
            trace_open!("full-text index read from the snapshot", _t0);
            snapshot_declarations = db
                .base
                .as_ref()
                .map(|b| b.declarations())
                .transpose()?
                .flatten();
        } else if !snap_header.is_empty() {
            // Legacy V5-V7: full read required for decode.
            let snap_bytes = db.fs.read(FileId::Snapshot)?;
            if let Some(state) = core_storage::snapshot::decode(&snap_bytes)? {
                db.restore_snapshot_state(state)?;
            }
        }
        // else: snap_header is empty = no snapshot file, fresh store. A store
        // with no snapshot has nothing for the lost-provenance repair to find.
        if snap_header.is_empty() {
            db.engine.mark_lost_provenance_reconciled();
        }
        //
        // Seed commit_seq from the highest seq persisted in last_change so that
        // WAL-replay frames (which start at commit_seq+1) always exceed any seq
        // already stored in the snapshot.  Without this, a db with one snapshot
        // commit would save last_change["a"]=1, then on reopen the first WAL
        // frame would replay at seq=1 again — colliding and making WAL-tail
        // mutations indistinguishable from the snapshot baseline.
        //
        // Safety invariant (seq-recycling):
        //   Recycled seqs (those below the seeded baseline) were NEVER stored in
        //   last_change because they belonged to a previous db lifetime — a new
        //   db starts at commit_seq=0 with an empty last_change.  Therefore no
        //   CAS precondition can carry a recycled seq as its `expected` value
        //   and accidentally match a live node's last_change entry.
        //
        // `expected:0` on a deleted-then-reinserted node:
        //   After deletion, last_changed() returns None; callers that call
        //   last_changed() and then use NodeUnchangedSince get None.unwrap_or(0)
        //   = 0.  The reinserted node gets seq > 0, so a subsequent CAS with
        //   expected=0 correctly conflicts.  The only way to observe actual=0 in
        //   a CasConflict would be a caller that invented expected=0 without ever
        //   calling last_changed() — unreachable via the documented API contract.
        if let Some(&max_seq) = db.last_change.values().max() {
            db.commit_seq = db.commit_seq.max(max_seq);
        }
        // Taken before the read: a file replaced between the two then reads
        // as a different identity later, which reloads — the safe direction.
        db.wal_ident = db.fs.wal_ident().map_err(GraphError::Io)?;
        let bytes = db.fs.read(FileId::Wal)?;
        let (records, valid_len) = decode_all(&bytes);
        // The valid prefix is replayed either way; `repair_wal` only decides
        // whether the truncation is written back. A reader that races a live
        // appender must not persist a truncation the writer never asked for.
        if valid_len < bytes.len() && repair_wal {
            db.fs.write_atomic(FileId::Wal, &bytes[..valid_len])?;
            db.wal_ident = db.fs.wal_ident().map_err(GraphError::Io)?;
        }
        // WAL-present path: build indexes eagerly BEFORE replay so that the
        // first replayed record does not trigger the lazy-init guard (which
        // would call reindex_all_load_state on an empty graph, defeating the
        // point of restoring IVF/HNSW blobs from the snapshot).
        if !records.is_empty() {
            db.ensure_v8_base_sections_loaded();
            trace_open!("lazy sections loaded (WAL path)", _t0);
        }
        // Scoped to this call: `refresh()` also replays frames and is *not*
        // followed by a rebuild, so its backfill must still run.
        //
        // And only when a rebuild does follow. An index read from the snapshot
        // is kept, so a pair declared after that snapshot is indexed by its
        // own backfill or not at all.
        // Every decoded frame occupies an index, replayed or not: markers
        // are state no-ops but they are not index no-ops.
        let decoded_frames = records.len() as u64;
        // A snapshot that kept its WAL says how much of it it already holds.
        // Those frames are counted, and not applied a second time.
        //
        // So does the intent of a snapshot that was to replace its WAL and
        // did not get that far: the same state, reached by a crash.
        let (interrupted, intent_left_over) = db.interrupted_snapshot(&bytes)?;
        // An intent that is not about this snapshot and this WAL is left over
        // from a snapshot that finished, or never wrote its snapshot. It has
        // to go before this handle appends: once new frames follow, a WAL can
        // come to begin with the bytes an old intent names. It goes at the
        // first append and not here, because only an appender holds the write
        // lock, and a handle without it may be looking at a peer's snapshot
        // half way through.
        db.intent_unsettled = intent_left_over;
        let held = db
            .base
            .as_ref()
            .and_then(|base| base.wal_mark())
            .or(interrupted)
            .filter(|mark| mark.matches(&bytes))
            .map(|mark| (mark.len as usize, decode_all(&bytes[..mark.len as usize])))
            // The mark was taken at the end of a whole WAL, so it ends on a
            // frame boundary; one that does not is not this WAL's.
            .filter(|(len, (_, consumed))| consumed == len)
            .map(|(_, (held, _))| held.len());
        let marked_wal_is_here = held.is_some();
        let held = held.unwrap_or(0);
        // A snapshot that kept its WAL has its index read on the strength of
        // its mark. If the WAL beside it is not the one marked — it lost its
        // tail, or was repaired shorter — the whole of it replays over the
        // snapshot, and what that produces is not what the index was read
        // for: a replayed insert can relabel a node and leave a posting under
        // a field its new label does not index. The index is set aside and
        // built after the replay, as it was before it could be read.
        if db.fulltext_loaded && !db.base_wal_truncated && !marked_wal_is_here {
            db.fulltext = Arc::new(core_storage::fulltext::FulltextIndex::new());
            db.fulltext_loaded = false;
        }
        db.fulltext_rebuild_follows = !db.fulltext_loaded;
        db.fulltext_reindexed_a_node = false;
        db.replaying_at_open = true;
        // The declarations a V11 snapshot carries (section 18) are the state at
        // the point of the WAL the snapshot was taken at, so they are applied
        // there: after the `held` frames, before the first frame the snapshot
        // does not hold. Earlier would let a held declaration that was later
        // replaced be skipped as already declared, and the reopened store
        // would differ from the never-closed one. For a WAL that was replaced
        // (`held == 0`) the WAL repeats them as its baseline, and for one
        // archived away before its baseline was written they are all there is.
        // A declaration is never checked against the data.
        let section_recs = snapshot_declarations
            .as_ref()
            .map(declarations_as_records)
            .unwrap_or_default();
        let replayed = db.apply_frames_at(records, held, section_recs);
        db.fulltext_rebuild_follows = false;
        db.replaying_at_open = false;
        let replayed = replayed?;
        // ── The multiplicity declaration, recovered from the stamp ───────────
        //
        // The opt-in is re-emitted into every baseline WAL a snapshot writes, so
        // ordinarily the replay above has already found it. But
        // `snapshot_with(archive_wal)` renames the live WAL away and writes its
        // replacement afterwards, and between those two points the store holds
        // no live declaration at all. A crash there — or a single `Err` from any
        // call in between — used to opt the store back out on the next open
        // (defect #22): it would stop counting and write a **V9** snapshot while
        // the archives still carried discriminant 23, which is the exact state
        // the V10 stamp exists to prevent.
        //
        // The V10 stamp is what carries the conclusion. The archive clause is a
        // scope restriction, not a second proof — an earlier version of this
        // comment, and defect #22, claimed otherwise, and defect #33 corrects
        // it. Taking the two in order:
        //
        // **The stamp.** `snapshot_with` stamps the snapshot from
        // `self.multiplicity` *before* it touches the WAL, and nothing rewrites
        // a V10 snapshot at V9 while the store believes it is opted in. So a
        // V10 stamp says this store reached `enable_multiplicity` far enough to
        // write the snapshot — and, decisively, that every older binary already
        // refuses this store by name. Opting in here can cost such a reader
        // nothing it was not already being told.
        //
        // **What the archive clause does not prove.** It is *not* evidence that
        // the archive was taken while the store was opted in. A store can
        // archive at V9 and opt in afterwards, leaving a V10 snapshot standing
        // beside an archive whose WAL carries no declaration at all — see
        // `a_failed_opt_in_beside_an_archive_comes_back_opted_in`. The inference
        // held in the success case by coincidence, not by construction.
        //
        // **What it does buy: scope.** Without it the recovery would also fire
        // on a store that reached the V10 snapshot write and then failed with no
        // archive in sight. That store must stay opted out, and can: no WAL was
        // renamed away, nothing carries discriminant 23, and its next snapshot
        // rewrites at V9, which puts it back within reach of every older reader.
        // An archive is the marker for the one state that is not recoverable
        // that way — a WAL renamed away that may hold the only copy of the
        // declaration. `no_crash_leaves_discriminant_23_unguarded` pins that
        // line: it sweeps a workload with no archives at all and refuses a
        // V10-implies-enabled rule.
        //
        // **The invariant, whichever way the clause goes:** the recovery never
        // opts in a store whose snapshot is not V10. A V9 store has made no
        // promise to an older reader, so opting it in would start writing
        // discriminant 23 behind a stamp that does not guard it. Pinned by
        // `the_recovery_never_opts_in_a_store_whose_snapshot_is_not_v10` and
        // `the_recovery_does_not_opt_a_store_in_by_itself`.
        //
        // What this recovery cannot do is make the opt-in atomic; it is not,
        // and `enable_multiplicity` says so. See defects #32-#34.
        //
        // A V11 store says it in section 18 rather than with its version: V11
        // is above V10, and a store with any V11 feature writes V11 whether it
        // counts multiplicity or not. The section's flag is written from the
        // same `self.multiplicity` the V10 stamp is, so it carries exactly the
        // fact the stamp did, and the recovery is the same one.
        let stamped_multiplicity = snapshot_version == Some(core_storage::snapshot::VERSION_10)
            || snapshot_declarations
                .as_ref()
                .is_some_and(|d: &core_storage::v8::SnapshotDeclarations| d.multiplicity);
        if !db.multiplicity && stamped_multiplicity && !db.fs.list_archives()?.is_empty() {
            db.multiplicity = true;
        }
        // The cursor sits at the end of the valid prefix, not the end of the
        // file: a torn or still-being-written tail is unconsumed by definition
        // and stays visible to `is_stale` until it decodes.
        db.wal_consumed = valid_len as u64;
        // The frame cursor counts the same sequence `all_frames` returns:
        // surviving archives first, then the live WAL, offset by the floor.
        // Counting the archives separately rather than calling
        // `wal_total_commits` keeps the live WAL from being decoded twice on
        // every open, and costs nothing on a store that has never archived.
        db.wal_frames_written = db.wal_horizon_floor + db.archive_frame_count()? + decoded_frames;
        db.snapshot_ident = db.fs.snapshot_ident().map_err(GraphError::Io)?;
        // What was just replayed is not this handle's fsync to vouch for: a
        // reader of the durable position fsyncs it once before counting it.
        db.durable_wal.loaded(
            db.snapshot_ident,
            db.wal_ident,
            db.wal_frames_written - decoded_frames,
            db.wal_consumed,
            db.wal_frames_written,
        );
        trace_open!("wal replay done", _t0);
        // Rebuild view values after WAL replay only when there is no V8 base.
        // With a V8 base, view values are correct in the snapshot and are updated
        // incrementally during WAL replay (on_edge_changed / on_prop_changed).
        // A full rebuild would read overlay-only props (empty after restore_v8_base)
        // and overwrite correct base values with wrong results (e.g. NeighborAgg
        // Sum reads no "score" in overlay → writes 0.0, shadowing the correct
        // base value).
        if db.base.is_none() {
            let topo_view = TopologyView::owned(&db.topo);
            db.view_store.rebuild_all(
                Arc::make_mut(&mut db.props),
                &topo_view,
                &db.ids,
                &db.syms,
                &db.labels,
            );
        }
        // The full-text index after WAL replay. One read from the snapshot has
        // had the replay applied to it record by record and is kept — unless
        // the replay indexed a node the snapshot already held, which doubles
        // that node's postings. Then, and wherever no index was read, it is
        // built from the declared pairs and the stored values.
        db.fulltext_loaded = db.fulltext_loaded && !db.fulltext_reindexed_a_node;
        if db.fulltext_loaded {
            #[cfg(debug_assertions)]
            {
                let mut rebuilt = (*db.fulltext).clone();
                rebuilt.rebuild_all(
                    &db.ids,
                    &db.labels,
                    &db.syms,
                    build_props_view(&db.props, &db.base),
                );
                debug_assert!(
                    rebuilt == *db.fulltext,
                    "the full-text index read from the snapshot is not the one a rebuild gives"
                );
            }
        } else {
            Arc::make_mut(&mut db.fulltext).rebuild_all(
                &db.ids,
                &db.labels,
                &db.syms,
                build_props_view(&db.props, &db.base),
            );
        }
        trace_open!("full-text index ready", _t0);
        db.drop_indexes_over_view_properties();
        Arc::make_mut(&mut db.prop_index).rebuild_all(
            &db.ids,
            &db.labels,
            &db.syms,
            build_props_view(&db.props, &db.base),
        );
        // Namespaces: one pass over the `ns` column, after the snapshot is
        // restored and the WAL replayed. Replay maintains `node_ns` record by
        // record as well; this pass is what makes a snapshot-only open right,
        // and it reads nothing on a store with no `ns` column.
        db.rebuild_node_ns();
        // A mid-build snapshot's HNSW blob carries `complete == false`.
        // Register it so `serve`'s ticker sees work without waiting for a write.
        db.register_outstanding_index_builds();
        // Load roles sidecar. Missing file = no roles (Some(vec![])).
        // Corrupt/unparseable = poisoned (None); mask_for_role will fail-loud.
        db.roles = Self::load_roles_from_fs(&db.fs)?;
        // The time sidecar. Absent is the normal case for any store written
        // before v0.6.11 and is not an error; unreadable is recorded so date
        // queries can say "damaged" rather than "none recorded".
        db.load_commit_times_from_fs();
        // Capture the initial MVCC fold so reader() is ready immediately.
        db.fold_now();
        trace_open!("open_with complete", _t0);
        Ok(replayed)
    }

    /// Apply decoded WAL frames to in-memory state, exactly as the open-path
    /// replay does — same `apply` calls, same per-frame delta drain, same
    /// commit-seq and last-change bookkeeping. Rules therefore fire and derived
    /// edges appear identically whether a frame arrives at open, from a local
    /// commit, or from another process by way of [`refresh`](GraphDb::refresh).
    ///
    /// Returns the number of frames applied.
    ///
    /// Deltas are drained and discarded per frame: replayed frames are already
    /// reflected on disk, so they are not news to a subscriber, and draining
    /// inside the loop keeps `pending_deltas` O(1) over a large WAL (I-2).
    fn apply_frames(&mut self, records: Vec<WalRecord>) -> Result<usize> {
        self.apply_frames_over(records, 0)
    }

    /// [`Self::apply_frames_over`], applying `at_held` — the declarations a
    /// snapshot carries — at the `held` boundary, the point of the WAL the
    /// snapshot was taken at.
    fn apply_frames_at(
        &mut self,
        records: Vec<WalRecord>,
        held: usize,
        at_held: Vec<WalRecord>,
    ) -> Result<usize> {
        if at_held.is_empty() {
            return self.apply_frames_over(records, held);
        }
        let held = held.min(records.len());
        let mut records = records;
        let after = records.split_off(held);
        let before_count = records.len();
        // The held prefix is numbered and its declarations applied, exactly
        // as `apply_frames_over` would; nothing in it is otherwise applied.
        let mut applied = self.apply_frames_over(records, before_count)?;
        for rec in &at_held {
            self.apply(rec)?;
        }
        applied += self.apply_frames_over(after, 0)?;
        Ok(applied)
    }

    /// [`Self::apply_frames`] over a base that already holds the first `held`
    /// records: the state a snapshot that kept its WAL was taken at. Those
    /// are numbered like any other frame and not applied, because applying a
    /// write to the state it produced is not always a no-op — an insert
    /// under a key since renamed or deleted makes a second node. A
    /// declaration among them is applied all the same: the pairs a store has
    /// declared are known from the WAL — and, when a snapshot carries a
    /// full-text index that is read, from that index too.
    fn apply_frames_over(&mut self, records: Vec<WalRecord>, held: usize) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        // Materialize any state retained in the mmap base before the first
        // frame lands, so a replayed record cannot trip the lazy-init guard and
        // rebuild indexes from an empty graph. Both calls are idempotent.
        //
        // Not before a declaration, though. Every truncating snapshot re-emits
        // one record for each declared pair, so a snapshotted store with any
        // declaration always has frames to replay; counting those as writes
        // made every open of such a store pay the scan a store with no
        // declaration defers to its first write.
        let mut retained_consumed = false;
        let applied = records.len();
        // A live commit turns on the engine's report of the edges its rules
        // add and retract, which is what keeps a view over a derived edge
        // type current. Replay has to do the same, or a store with a snapshot
        // — where nothing recomputes the views afterwards — reopens with the
        // counts the snapshot had.
        let emitted_before = self.engine.emit_deltas();
        for (i, rec) in records.into_iter().enumerate() {
            if !self.view_store.is_empty() && !self.engine.emit_deltas() {
                self.engine.set_emit_deltas(true);
            }
            // A frame the snapshot already holds is counted and not applied,
            // unless it is a declaration.
            let apply = i >= held || is_declaration(&rec);
            if apply && !retained_consumed && !is_declaration(&rec) {
                self.ensure_v8_base_sections_loaded();
                self.engine.consume_retained_state_eager(
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    build_props_view(&self.props, &self.base),
                );
                retained_consumed = true;
            }
            if apply && !is_declaration(&rec) {
                self.reconcile_lost_provenance();
            }
            if apply {
                self.apply(&rec)?;
            }
            let _ = self.engine.drain_deltas();
            // Track commit_seq during replay so last_change entries are
            // consistent with the seqs assigned by log_then_apply_with on
            // subsequent live commits.  After N replayed frames, commit_seq=N;
            // live commits begin at N+1.
            self.commit_seq += 1;
            let replay_seq = self.commit_seq;
            self.update_last_change_from_rec(&rec, replay_seq);
        }
        // Enforce I-2: if the per-frame drain above is ever removed or skipped,
        // this assert catches the regression in debug builds immediately.
        debug_assert_eq!(
            self.engine.pending_delta_count(),
            0,
            "pending_deltas non-empty after replay — \
             per-frame drain must run inside the loop to keep memory O(1)"
        );
        // T2 note: the per-frame drain IS the suppression seam for replay.
        // Any future as-of replay path (Plan-15 T2) must drain here to feed
        // replaying subscribers; the mechanism is already in place.
        let _ = self.engine.drain_deltas(); // belt-and-braces no-op after loop drain
        self.engine.set_emit_deltas(emitted_before);
        Ok(applied)
    }

    // ── Multi-process safety: cross-process write lock + WAL tailing ──────────
    //
    // mushroomdb is many-readers / one-writer across processes. Writers take an
    // advisory exclusive lock on the store's `LOCK` file; readers never do.
    // Every handle tracks how much of the WAL it has consumed, so it can pick
    // up another process's commits by decoding only the new tail rather than
    // reopening. See `docs/site/concurrency.md`.

    /// Whether the store on disk has moved ahead of (or out from under) this
    /// handle's in-memory state.
    ///
    /// True when the WAL's length differs from this handle's cursor — another
    /// process committed, or is mid-append — or when the snapshot file's
    /// identity changed. Costs two metadata lookups and reads no file contents,
    /// so it is cheap enough for a read path to call.
    ///
    /// Always false for an as-of view from [`GraphDb::open_at`]: such a view is
    /// pinned to one commit and later commits are deliberately invisible to it.
    pub fn is_stale(&self) -> Result<bool> {
        self.disk_mark().is_stale_on(&self.fs)
    }

    /// What [`Self::is_stale`] compares the store on disk against, as of now.
    ///
    /// Taken under the lock with a published snapshot, it lets a reader ask
    /// "is that snapshot still the store's state?" with no lock held.
    pub(crate) fn disk_mark(&self) -> DiskMark {
        DiskMark {
            pinned: self.pinned,
            wal_consumed: self.wal_consumed,
            snapshot_ident: self.snapshot_ident,
        }
    }

    /// Bring this handle up to date with every commit other processes have made,
    /// and return how many frames were applied.
    ///
    /// The WAL tail is decoded from this handle's cursor and applied through the
    /// same path the open replay uses, so rules fire and derived edges appear
    /// exactly as they would on a fresh open. Interners, id maps and indexes
    /// stay valid for the same reason.
    ///
    /// A frame another process is still writing is left alone: a trailing
    /// partial frame is a wait, not a corruption, and the handle stays stale
    /// until that frame is complete. Nothing is written to disk, so a read-only
    /// handle can refresh freely.
    ///
    /// When the snapshot file's identity changed, or the WAL is shorter than
    /// this handle's cursor, the WAL no longer continues our state — another
    /// process snapshotted or archived. The handle is then rebuilt from disk
    /// with the options it was opened with, and the return value is the number
    /// of frames in the new WAL.
    ///
    /// Returns 0 for an as-of view, which never follows later commits.
    ///
    /// # Errors
    ///
    /// An error here leaves the handle **degraded**: it got partway through
    /// applying the tail, or partway through a reload, so its in-memory state
    /// no longer matches any point on disk. Further mutations are refused and
    /// the handle must be reopened. Nothing on disk was damaged — the store
    /// itself is fine, and a fresh open recovers it.
    pub fn refresh(&mut self) -> Result<u64> {
        self.refresh_checking(false)
    }

    /// [`refresh`](GraphDb::refresh), with `before_append` set when a write
    /// scope is about to append at this handle's cursor.
    ///
    /// The case the WAL file's identity catches: the file was replaced while
    /// the snapshot this handle already saw stayed — it reloaded while another
    /// process was between writing its snapshot and replacing the WAL. Its
    /// cursor describes the old file. A tail read at that offset in the new
    /// one would apply frames that are not the next ones; an append there
    /// would write frames encoded against the wrong state, and the store would
    /// no longer open.
    ///
    /// A read follows a tail only when the WAL is longer than the cursor, so a
    /// read-only refresh checks the identity only then. An append happens at
    /// any length — a replacement of exactly the cursor's length included,
    /// which no length check can see — so before an append it is always
    /// checked: one stat per cross-process write scope.
    fn refresh_checking(&mut self, before_append: bool) -> Result<u64> {
        if self.pinned {
            return Ok(0);
        }
        let disk_ident = self.fs.snapshot_ident().map_err(GraphError::Io)?;
        // Before an append the WAL's identity is always checked, so both come
        // from one look at the file; otherwise only when the WAL grew.
        let (wal_len, appending_ident) = if before_append {
            let (len, ident) = self.fs.wal_len_and_ident().map_err(GraphError::Io)?;
            (len, Some(ident))
        } else {
            (self.fs.wal_len().map_err(GraphError::Io)?, None)
        };
        let wal_replaced = || -> Result<bool> {
            let ident = match appending_ident {
                Some(ident) => ident,
                None if wal_len > self.wal_consumed => {
                    self.fs.wal_ident().map_err(GraphError::Io)?
                }
                None => return Ok(false),
            };
            Ok(ident != self.wal_ident)
        };
        if disk_ident != self.snapshot_ident || wal_len < self.wal_consumed || wal_replaced()? {
            // The WAL no longer continues our state: rebuild from disk. State
            // is cleared first, so a failed load leaves an empty handle — mark
            // it degraded rather than let a caller read an empty graph as if
            // it were the store's contents.
            //
            // The WAL this handle was reading is not the one on disk any more,
            // so the durable position starts a new generation; what a peer
            // fsynced is not this handle's to vouch for.
            self.durable_wal.replaced();
            self.reset_for_reload();
            return match self.load_from_disk(LoadOrigin::Reload) {
                Ok(frames) => Ok(frames as u64),
                Err(e) => {
                    self.degraded = true;
                    Err(e)
                }
            };
        }
        if wal_len == self.wal_consumed {
            return Ok(0);
        }
        let tail = self
            .fs
            .read_range(FileId::Wal, self.wal_consumed)
            .map_err(GraphError::Io)?;
        let (records, valid_len) = decode_all(&tail);
        let decoded_frames = records.len() as u64;
        let applied = match self.apply_frames(records) {
            Ok(n) => n,
            Err(e) => {
                // Some frames landed and some did not, and the cursor cannot
                // say how many. Advancing it would skip the rest; leaving it
                // would replay what already applied. Neither is recoverable in
                // place, so refuse further writes and require a reopen.
                self.degraded = true;
                return Err(e);
            }
        };
        // Advance by the bytes actually decoded, never by the file length: an
        // incomplete trailing frame stays unconsumed for the next refresh.
        self.wal_consumed += valid_len as u64;
        self.wal_frames_written += decoded_frames;
        // Another process wrote these frames; whether it fsynced them is not
        // this handle's to say.
        self.durable_wal
            .absorbed(self.wal_consumed, self.wal_frames_written);
        // The peer that wrote those frames also stamped them. Absorbing the
        // frames without the stamps leaves this handle resolving dates from a
        // prefix of the store's history, and — while our own map is still
        // empty — one commit away from rewriting the peer's file out of
        // existence (`first` below decides on the map, and the map is what we
        // just brought up to date).
        self.load_commit_times_from_fs();
        if applied > 0 {
            // Peer commits must reach `reader()` snapshots taken from here on.
            // A full fold is what open does; refresh does not build per-commit
            // deltas, so there is nothing cheaper that stays correct.
            self.fold_now();
        }
        Ok(applied as u64)
    }

    /// Byte offset of the WAL prefix this handle has applied.
    ///
    /// Exposed for tests that assert the cursor tracks appended bytes exactly.
    #[doc(hidden)]
    pub fn wal_consumed(&self) -> u64 {
        self.wal_consumed
    }

    /// Rewind the WAL cursor after the group-commit drain thread truncated a
    /// failed group off the tail, so the cursor still describes the file.
    pub(crate) fn set_wal_consumed(&mut self, len: u64) {
        self.wal_consumed = len;
    }

    /// The global frame index the next appended frame will occupy. The frame
    /// twin of [`wal_consumed`](GraphDb::wal_consumed).
    pub(crate) fn wal_frames_written(&self) -> u64 {
        self.wal_frames_written
    }

    /// How much of the live WAL is known to be on stable storage — only bytes
    /// an fsync has covered, ending on a frame boundary. See [`WalPosition`].
    ///
    /// Under [`FsyncPolicy::Relaxed`] nothing is fsynced between snapshots, so
    /// the position advances only when a snapshot replaces the WAL; under
    /// [`FsyncPolicy::Batched`] it advances at the next batch fsync.
    pub fn durable_wal_position(&self) -> WalPosition {
        self.durable_wal.get()
    }

    /// The shared cell behind [`durable_wal_position`](GraphDb::durable_wal_position),
    /// for a [`SharedDb`](crate::SharedDb) to read without the handle's lock.
    pub(crate) fn durable_wal(&self) -> DurableWal {
        self.durable_wal.clone()
    }

    /// Record that everything this handle has appended is now fsynced.
    fn note_wal_synced(&self) {
        self.durable_wal
            .synced(self.wal_consumed, self.wal_frames_written);
    }

    /// One non-blocking attempt at the cross-process write lock.
    ///
    /// Takes `&self` so a caller can poll for the lock *before* it acquires the
    /// in-process write guard. That ordering is what keeps a busy peer in
    /// another process from stalling this process's readers.
    ///
    /// A handle that owns the lock for its lifetime always succeeds.
    pub(crate) fn try_cross_process_lock(&self) -> Result<bool> {
        if self.holds_lifetime_lock {
            return Ok(true);
        }
        self.fs.try_lock_exclusive().map_err(|e| {
            // A filesystem that refuses to create or open `LOCK` is a deployment
            // fault, not a peer holding the lock: say which, so the caller never
            // reports it as `Busy`.
            match (
                core_storage::fs::is_permission_refusal(&e),
                self.fs.wal_path(),
            ) {
                (true, Some(wal)) => match wal.parent() {
                    Some(dir) => core_storage::fs::unwritable_error(
                        dir,
                        &dir.join(core_storage::fs::LOCK_FILE),
                        &e,
                    ),
                    None => GraphError::Io(e),
                },
                _ => GraphError::Io(e),
            }
        })
    }

    /// Poll for the cross-process write lock until `wait` elapses.
    ///
    /// One attempt is always made, so a zero wait is a single try. Returns
    /// `false` when the lock is still held elsewhere at the deadline; nothing
    /// has been written and retrying later is safe.
    ///
    /// Only the plain-`GraphDb` open path uses this, where the caller owns the
    /// handle outright. [`SharedDb`](crate::SharedDb) polls
    /// [`try_cross_process_lock`](GraphDb::try_cross_process_lock) itself so
    /// that it holds no in-process guard while it waits.
    fn poll_lock(&self, wait: std::time::Duration) -> Result<bool> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            if self.try_cross_process_lock()? {
                return Ok(true);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            std::thread::sleep(LOCK_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
        }
    }

    /// Open a cross-process write scope, given the outcome of an already-made
    /// lock attempt.
    ///
    /// The caller polls for the lock first — outside any in-process guard — and
    /// passes what it got. On success this refreshes, so the writes about to
    /// happen land on top of every other process's commits. On failure the
    /// handle refuses WAL-appending mutations and `snapshot()` with
    /// [`GraphError::Busy`] until [`end_write_lock`](GraphDb::end_write_lock)
    /// closes the scope, so a caller holding a guard cannot write behind
    /// another process's back.
    ///
    /// A handle that already owns the lock for its lifetime skips the refresh:
    /// no other process can have written, so there is nothing to pick up.
    pub(crate) fn enter_write_scope(&mut self, acquired: bool) -> Result<()> {
        self.lock_denied = !acquired;
        self.lock_failure = None;
        if !acquired || self.holds_lifetime_lock {
            return Ok(());
        }
        if let Err(e) = self.refresh_checking(true) {
            // Do not hold a lock we cannot use: release it and let the caller
            // see the underlying failure.
            let _ = self.fs.unlock();
            self.lock_denied = true;
            return Err(e);
        }
        Ok(())
    }

    /// Record why a lock attempt failed, for the scope [`enter_write_scope`]
    /// (with `false`) is about to open: an [`GraphError::Unwritable`] or
    /// [`GraphError::Io`] is kept and reported instead of [`GraphError::Busy`].
    pub(crate) fn note_lock_failure(&mut self, err: &GraphError) {
        self.lock_failure = copy_lock_failure(err);
    }

    /// The error a mutation returns while the write scope is denied.
    fn lock_denied_error(&self) -> GraphError {
        self.lock_failure
            .as_ref()
            .and_then(copy_lock_failure)
            .unwrap_or(GraphError::Busy { holder: None })
    }

    /// Close a cross-process write scope opened by
    /// [`enter_write_scope`](GraphDb::enter_write_scope): release the lock and
    /// clear the Busy latch. Safe to call when the lock was never taken.
    pub(crate) fn end_write_lock(&mut self) {
        self.lock_denied = false;
        self.lock_failure = None;
        if !self.holds_lifetime_lock {
            // Releasing a lock we do not hold is a no-op; a failure to release
            // is reported by the OS closing the descriptor at handle drop.
            let _ = self.fs.unlock();
        }
    }

    /// As-of replay for [`GraphDb::open_at`]: snapshot base (only when the
    /// snapshot truncated the WAL) plus the first `commit + 1` WAL frames;
    /// see [`GraphDb::open_at`] for the semantics.  The per-frame drain
    /// mirrors `open_with` exactly so pending_delta_count is 0 on exit.
    /// Restore all persisted state from a decoded snapshot. Shared by
    /// `open_with` and (when the snapshot truncated the WAL) `open_at_with`.
    fn restore_snapshot_state(
        &mut self,
        state: core_storage::snapshot::SnapshotState,
    ) -> Result<()> {
        self.ids = Arc::new(state.ids);
        self.syms = Arc::new(state.syms);
        self.topo = Arc::new(state.topo);
        self.props = Arc::new(state.props);
        self.labels = Arc::new(state.labels);
        self.edge_props = Arc::new(state.edge_props);
        // Cross-section label integrity for V5/V7 snapshots: same invariants as
        // restore_v8_base.  A crafted bincode snapshot with a short `labels` vec,
        // out-of-range sym ids, or a sentinel label on a live node would otherwise
        // open successfully and panic later in `NodeRef::label()` or
        // `neighborhood_masked()`.  Catching it here turns those into typed
        // `GraphError::Corrupt` at open time.
        {
            let ids_len = self.ids.len();
            if self.labels.len() != ids_len {
                return Err(GraphError::Corrupt {
                    detail: format!(
                        "snapshot: labels vec has {} entries but id table has {} total slots",
                        self.labels.len(),
                        ids_len,
                    ),
                });
            }
            let syms_len = self.syms.len() as u32;
            for (i, &sym) in self.labels.iter().enumerate() {
                let is_tombstoned = self.ids.is_tombstoned(i as u32);
                if sym == u32::MAX {
                    if !is_tombstoned {
                        return Err(GraphError::Corrupt {
                            detail: format!(
                                "snapshot: live node at id slot {i} has sentinel label (u32::MAX)"
                            ),
                        });
                    }
                } else if sym >= syms_len {
                    return Err(GraphError::Corrupt {
                        detail: format!(
                            "snapshot: label at id slot {i} references sym {sym} \
                             which is out of interner range ({syms_len})"
                        ),
                    });
                }
            }
        }
        let defs: Vec<RuleDef> = state
            .rule_defs
            .iter()
            .map(|b| {
                decode_rule_def(b).map_err(|e| GraphError::Corrupt {
                    detail: format!("snapshot rule_def deserialize: {e}"),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.engine =
            RuleEngine::from_persist(defs, state.provenance, state.rule_tripped, state.rule_fires);
        self.engine_from_base = false;
        // Candidate indexes are rebuilt lazily on the first mutation (see
        // RuleEngine::on_node_changed).  HNSW blobs and IVF centroids from the
        // snapshot are retained without deserializing so that:
        //   - clean-open (empty WAL): indexes stay empty; blobs load on first
        //     ANN query via ensure_hnsw_loaded, or on first mutation via the
        //     lazy-init guard which calls reindex_all_load_state (the scan
        //     skips the HNSW build for every side the blob supplies).
        //   - WAL-present: open_with calls consume_retained_state_eager before
        //     replay so HNSW/IVF are live before any record fires the hooks.
        let ivf_bytes = if state.ivf_state.is_empty() {
            Vec::new()
        } else {
            bincode::serialize(&state.ivf_state).expect("IVF state serialize cannot fail")
        };
        // Store blobs without eagerly deserializing them.
        // `self.ids` is the snapshot's id table at this point — WAL replay has
        // not run — so its length is the line an interrupted build is detected
        // against.
        let snapshot_ids = self.ids.len() as u32;
        self.engine
            .store_snapshot_state(state.hnsw_state, ivf_bytes, snapshot_ids);
        // Restore view defs from snapshot (V5).
        // The ColumnStore already contains view values from the snapshot;
        // use restore_view (no collision check, no backfill) so the store
        // is aware of the definitions.  rebuild_all runs after WAL replay.
        for def_bytes in &state.view_defs {
            let def: ViewDef =
                bincode::deserialize(def_bytes).map_err(|e| GraphError::Corrupt {
                    detail: format!("snapshot view_def deserialize: {e}"),
                })?;
            self.view_store
                .restore_view(def)
                .map_err(|e| GraphError::Corrupt {
                    detail: format!("snapshot view restore: {e}"),
                })?;
        }
        Ok(())
    }

    /// Restore all persisted state from a V8 `MappedBase` snapshot, **except**
    /// topology (`self.topo` stays empty and serves as the WAL-replay overlay).
    ///
    /// `self.props` IS fully materialised from the base so that HNSW/IVF blob
    /// deserialization and view rebuild have access to all column data.
    fn restore_v8_base(&mut self, mapped: Arc<core_storage::v8::MappedBase>) -> Result<()> {
        self.ids = Arc::new(archived_to_idmap(mapped.ids().map_err(|e| {
            GraphError::Corrupt {
                detail: format!("v8: ids section: {e:?}"),
            }
        })?));
        self.syms = Arc::new(archived_to_interner(mapped.syms().map_err(|e| {
            GraphError::Corrupt {
                detail: format!("v8: syms section: {e:?}"),
            }
        })?));

        // C1: self.props is left as an empty overlay. Column reads go through
        // props_view() (ColumnsView::with_base), which consults the archived base
        // section zero-copy. This avoids the O(columns) heap copy at every open.

        // self.topo deliberately left as Topology::new() — overlay path.

        let meta = decode_meta(mapped.meta_bytes().map_err(|e| GraphError::Corrupt {
            detail: format!("v8: meta section: {e:?}"),
        })?)
        .map_err(|e| GraphError::Corrupt {
            detail: format!("v8: meta decode: {e:?}"),
        })?;
        self.base_wal_truncated = meta.wal_truncated;
        self.labels = Arc::new(meta.labels);
        // Cross-section label integrity: labels must cover every id slot (live
        // and tombstoned), every non-sentinel sym must be within the interner's
        // bound, and no live (non-tombstoned) node may carry the u32::MAX
        // sentinel label.  Without this check, a crafted snapshot where the META
        // section (small, CRC-validated) holds a short `labels` vec, out-of-range
        // sym ids, or a sentinel label on a live node, would open successfully
        // and then panic in `NodeRef::label()`, `neighborhood_masked()`, and
        // related read paths.  Catching the inconsistency here converts those
        // panics into typed `GraphError::Corrupt` at open time.
        {
            let ids_len = self.ids.len();
            if self.labels.len() != ids_len {
                return Err(GraphError::Corrupt {
                    detail: format!(
                        "v8: labels section has {} entries but id table has {} total slots",
                        self.labels.len(),
                        ids_len,
                    ),
                });
            }
            let syms_len = self.syms.len() as u32;
            for (i, &sym) in self.labels.iter().enumerate() {
                let is_tombstoned = self.ids.is_tombstoned(i as u32);
                if sym == u32::MAX {
                    // Sentinel is only valid for tombstoned slots.
                    if !is_tombstoned {
                        return Err(GraphError::Corrupt {
                            detail: format!(
                                "v8: live node at id slot {i} has sentinel label (u32::MAX)"
                            ),
                        });
                    }
                } else if sym >= syms_len {
                    return Err(GraphError::Corrupt {
                        detail: format!(
                            "v8: label at id slot {i} references sym {sym} \
                             which is out of interner range ({syms_len})"
                        ),
                    });
                }
            }
        }
        // C3: self.edge_props stays as an empty overlay.  Reads go through
        // edge_props_view() which consults the mmap'd base section zero-copy
        // via EdgePropsView::with_base.  No heap decode at open time.

        // Restore rule engine.
        let (rule_def_bytes, rule_tripped, rule_fires) =
            archived_rules_meta_to_owned(mapped.rules_meta_section().map_err(|e| {
                GraphError::Corrupt {
                    detail: format!("v8: rules_meta section: {e:?}"),
                }
            })?);
        let defs: Vec<RuleDef> = rule_def_bytes
            .iter()
            .map(|b| {
                decode_rule_def(b).map_err(|e| GraphError::Corrupt {
                    detail: format!("v8: rule_def deserialize: {e}"),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.engine = RuleEngine::from_persist(defs, BTreeMap::new(), rule_tripped, rule_fires);
        self.engine_from_base = true;
        // C4+C5: provenance, HNSW, and IVF sections are NOT read here.
        // `ensure_v8_base_sections_loaded` reads them on first use from
        // `self.base` (set by the caller immediately after this returns).
        // A clean open touches only: header + IDS + SYMS + META + RULES_META.

        // Restore view definitions.
        let view_defs =
            archived_views_to_owned(mapped.views_section().map_err(|e| GraphError::Corrupt {
                detail: format!("v8: views section: {e:?}"),
            })?);
        for def_bytes in &view_defs {
            let def: ViewDef =
                bincode::deserialize(def_bytes).map_err(|e| GraphError::Corrupt {
                    detail: format!("v8: view_def deserialize: {e}"),
                })?;
            self.view_store
                .restore_view(def)
                .map_err(|e| GraphError::Corrupt {
                    detail: format!("v8: view restore: {e}"),
                })?;
        }
        // Load the last-change map from section 11 (small section; load eagerly).
        // Pre-Task-3 snapshots lack this section; `last_change_bytes` returns &[]
        // in that case and `decode_last_change_bytes` returns an empty map.
        let last_change_raw = mapped
            .last_change_bytes()
            .map_err(|e| GraphError::Corrupt {
                detail: format!("v8: last_change section: {e:?}"),
            })?;
        self.last_change = decode_last_change_bytes(last_change_raw);

        // Validate that all deferred sections (provenance, HNSW, IVF) fit within
        // the file.  Pure bounds check — no bytes read, no page faults triggered.
        // Catches truncated snapshots at open time before the lazy deferred reads.
        mapped.validate_section_bounds().map_err(|e| match e {
            GraphError::Corrupt { detail } => GraphError::Corrupt {
                detail: format!("v8: section bounds: {detail}"),
            },
            other => other,
        })?;

        // Count edges from the base CSR's actual out-rows rather than trusting
        // the snapshot header's `edge_count` field, which is wrong on stores
        // snapshotted while two rules owned one edge (ledger row 107).  The
        // topology section is already mmap'd; this is a single pass over the
        // row-length fields with no heap allocation.
        self.base_edge_count_from_rows = mapped.topology().ok().map(|csr| {
            csr.etypes
                .iter()
                .map(|e| {
                    e.out_adj
                        .rows
                        .iter()
                        .map(|r| r.neighbors.len() as u64)
                        .sum::<u64>()
                })
                .sum::<u64>()
        });

        Ok(())
    }

    /// Read provenance, HNSW, and IVF sections from the mmap base into the
    /// engine's retained fields on first call.  Subsequent calls are a no-op
    /// (AtomicBool fast-path).
    ///
    /// Must be called before any code path that reads or mutates engine
    /// provenance, HNSW, or IVF state:
    /// - WAL replay (before `consume_retained_state_eager`)
    /// - First mutation (`log_then_apply_with`)
    /// - Read-only paths (`stats`, `explain`, `node_edges`)
    /// - Snapshot (`snapshot_with`)
    ///
    /// No-op for fresh stores and V5-V7 opens (`self.base` is `None`).
    fn ensure_v8_base_sections_loaded(&self) {
        use std::sync::atomic::Ordering;
        if self.v8_sections_loaded.load(Ordering::Acquire) {
            return;
        }
        let _guard = self
            .v8_sections_mutex
            .lock()
            .expect("v8 sections mutex poisoned");
        if self.v8_sections_loaded.load(Ordering::Acquire) {
            return; // another caller populated while we waited
        }
        let _t = std::time::Instant::now();
        if let Some(base) = &self.base {
            // Provenance: raw rkyv bytes; CRC validated inside section_bytes.
            // Bounds are already validated at open time (restore_v8_base →
            // validate_section_bounds) — unreachable post-validate_section_bounds;
            // unwrap_or_default is a safety belt against impossible errors.
            let prov_bytes = base
                .provenance_raw_bytes()
                .map(|b| b.to_vec())
                .unwrap_or_default();
            self.engine.store_provenance_bytes(prov_bytes);
            // HNSW: decode rkyv blobs into owned map.
            let hnsw_state = base
                .hnsw_section()
                .map(archived_hnsw_to_owned)
                .unwrap_or_default();
            // IVF: raw bincode bytes; deserialized on first mutation/query.
            let ivf_bytes = base.ivf_bytes().map(|b| b.to_vec()).unwrap_or_default();
            // Called before WAL replay on a WAL-present open (`open_with`) and
            // before any write on a clean one, so this is the snapshot's count.
            let snapshot_ids = self.ids.len() as u32;
            self.engine
                .store_snapshot_state(hnsw_state, ivf_bytes, snapshot_ids);
        }
        self.v8_sections_loaded.store(true, Ordering::Release);
        if std::env::var("MUSHROOMDB_TRACE_OPEN").is_ok() {
            eprintln!(
                "[MUSHROOMDB_TRACE_OPEN] ensure_v8_base_sections_loaded: {:>9.3?}",
                _t.elapsed()
            );
        }
    }

    /// Return a `TopologyView` that merges the mmap'd base (when present) with
    /// the in-memory WAL overlay.  Used by all read paths in db.rs that need
    /// the full merged topology without going through `self.view()`.
    fn topo_view(&self) -> TopologyView<'_> {
        match self.base {
            None => TopologyView::owned(&self.topo),
            Some(ref base) => {
                // SAFETY: base lives as long as self; section bounds validated at open.
                // topology() uses access_unchecked; all field reads are bounds-checked in seam.rs.
                let archived = base
                    .topology()
                    .expect("base topology section bounds validated at open");
                TopologyView::with_base(&self.topo, archived)
            }
        }
    }

    /// Return a `ColumnsView` that merges the mmap'd base columns (when a V8
    /// snapshot is open) with the in-memory WAL overlay.  Reads consult the
    /// overlay first, then fall through to the archived base section zero-copy.
    fn props_view(&self) -> core_storage::v8::seam::ColumnsView<'_> {
        match self.base {
            None => core_storage::v8::seam::ColumnsView::owned(&self.props),
            Some(ref base) => {
                // columns() uses access_unchecked; field reads are bounds-checked in seam.rs.
                let archived = base
                    .columns()
                    .expect("base columns section bounds validated at open");
                core_storage::v8::seam::ColumnsView::with_base_cached(
                    &self.props,
                    archived,
                    base.mixed_cache(),
                )
                .with_shared_strings(base_string_table(base))
            }
        }
    }

    /// Return an `EdgePropsView` that merges the mmap'd base edge-props section
    /// (when a V8 snapshot is open) with the in-memory WAL overlay.
    ///
    /// Reads consult the overlay first (for post-snapshot mutations), then fall
    /// through to the archived base section zero-copy.  Tombstones in the
    /// overlay mask deleted-from-base entries.
    fn edge_props_view(&self) -> EdgePropsView<'_> {
        match self.base {
            None => EdgePropsView::owned(&self.edge_props),
            Some(ref base) => {
                // edge_props_section() uses access_unchecked; field reads bounds-checked in seam.rs.
                let archived = base
                    .edge_props_section()
                    .expect("base edge_props section bounds validated at open");
                EdgePropsView::with_base(&self.edge_props, archived)
            }
        }
    }

    fn open_at_with(fs: F, commit: u64) -> Result<Self> {
        // An as-of view never writes and is pinned to one commit: it takes no
        // cross-process lock and does not follow later commits.
        let mut db = Self::new_empty(
            fs,
            OpenOptions {
                repair_wal: false,
                auto_migrate: false,
                read_only: true,
            },
        );
        db.pinned = true; // read_only is set after replay, but pinning is immediate
        db.wal_horizon_floor = db.fs.read_horizon_floor()?;
        db.archive_genesis_chain = db.fs.has_genesis_marker();
        // Same orphaned-archive cleanup as open_with: floor was written first
        // during pruning, so a crash may have left stale archives below floor.
        db.cleanup_orphaned_archives()?;
        // Collect archive frames (oldest-first) and live WAL frames.
        // Archives represent pre-snapshot history; the snapshot captures the
        // cumulative state at the time of archiving.  Crash-window guarantee:
        //   A: crash before rename → WAL intact, no archive. Reopen: normal.
        //   B: crash after rename, before new WAL → archive present, WAL
        //      absent. Reopen: snapshot loaded (full state), no WAL replay.
        //   C: crash after new baseline WAL written → normal post-archive.
        let archive_ns = db.fs.list_archives()?;
        let mut archive_frames_all: Vec<WalRecord> = Vec::new();
        for n in &archive_ns {
            let arc_bytes = db.fs.read_archive(*n)?;
            let (arc_frames, _) = decode_all(&arc_bytes);
            archive_frames_all.extend(arc_frames);
        }
        let total_archive_frames = archive_frames_all.len() as u64;

        let live_bytes = db.fs.read(FileId::Wal)?;
        let (live_records, _valid_len) = decode_all(&live_bytes);
        let total_surviving = total_archive_frames + live_records.len() as u64;
        // Global total including any pruned history below the horizon floor.
        let total = db.wal_horizon_floor + total_surviving;

        // Horizon and range check.
        if commit < db.wal_horizon_floor {
            return Err(GraphError::CommitOutOfRange {
                commit,
                total,
                floor: db.wal_horizon_floor,
            });
        }
        if commit >= total {
            return Err(GraphError::CommitOutOfRange {
                commit,
                total,
                floor: db.wal_horizon_floor,
            });
        }

        // Local index into surviving frames (0 = first frame of oldest archive).
        let local = commit - db.wal_horizon_floor;

        if local < total_archive_frames {
            // Target commit is in an archive.  Correct replay from empty state
            // is only possible when the archive chain is an uninterrupted
            // genesis chain (first archive taken from a fresh store, no prior
            // WAL truncation) and no archives have been pruned (floor == 0).
            //
            // If either condition is violated the prefix needed to reconstruct
            // the requested state is gone; refuse rather than return wrong data.
            if db.wal_horizon_floor > 0 || !db.archive_genesis_chain {
                return Err(GraphError::CommitOutOfRange {
                    commit,
                    total,
                    floor: db.wal_horizon_floor,
                });
            }
            // Replay all archive frames up to and including the target commit
            // from an empty database state.  Archives must be replayed in order
            // so that dense-id intern tables are built up correctly.
            for rec in archive_frames_all.into_iter().take((local + 1) as usize) {
                // The repair a live commit ran before this one (ledger row 111).
                if !is_declaration(&rec) {
                    db.reconcile_lost_provenance();
                }
                db.apply(&rec)?;
                let _ = db.engine.drain_deltas();
            }
        } else {
            // Target commit is in the live WAL: load snapshot as base, then
            // replay the needed live WAL prefix.
            //
            // Base state: a truncating snapshot (wal_truncated=true) compacts
            // all pre-truncation / pre-archive commits.  Dense-id records in
            // the live WAL reference ids/interns that the snapshot provides.
            //
            // A `keep_wal` snapshot records `wal_truncated=false`: it is the
            // state at the head, not at the start of the WAL, so it is no base
            // for a prefix. That is right when the live WAL reaches the store's
            // first commit and can be replayed from nothing. It is wrong when
            // the WAL starts above that — the floor has moved, or archives sit
            // in front of it — because the snapshot that *was* its base has
            // been overwritten. Replaying the prefix over nothing then answers
            // with a graph missing everything older than the WAL. The base is
            // gone, so the read is refused (ledger row 82).
            let base_is_gone = db.wal_horizon_floor > 0 || total_archive_frames > 0;
            // A snapshot interrupted before its WAL was replaced is, like a
            // `keep_wal` one, the state at the END of the live WAL, whatever
            // it says about truncation: no base for a prefix of that WAL.
            let live_wal = db.fs.read(FileId::Wal)?;
            let snapshot_is_wal_end = db.interrupted_snapshot(&live_wal)?.0.is_some();
            // Peek 6 bytes (same pattern as open_with).
            let snap_header = db.fs.read_prefix(FileId::Snapshot, 6)?;
            // A V11 stamp makes the as-of handle V11 like a live one (C9),
            // whether or not the replayed prefix holds a V11 record.
            if snap_header.len() >= 6
                && &snap_header[0..4] == b"GDB1"
                && u16::from_le_bytes([snap_header[4], snap_header[5]])
                    >= core_storage::snapshot::VERSION_11
            {
                db.v11_features = true;
            }
            let is_v8 = snap_header.len() >= 6
                && &snap_header[0..4] == b"GDB1"
                && core_storage::snapshot::is_mmap_container(u16::from_le_bytes([
                    snap_header[4],
                    snap_header[5],
                ]));
            if is_v8 {
                let state = if let Some(snap_path) = db.fs.snapshot_path() {
                    let mapped = core_storage::v8::MappedBase::map(&snap_path).map_err(|e| {
                        GraphError::Corrupt {
                            detail: format!("v8: open_at mmap: {e:?}"),
                        }
                    })?;
                    core_storage::snapshot::decode_v8_from_mapped(&mapped)?
                } else {
                    let snap_bytes = db.fs.read(FileId::Snapshot)?;
                    core_storage::snapshot::decode(&snap_bytes)?
                };
                if let Some(state) = state {
                    if state.wal_truncated && !snapshot_is_wal_end {
                        db.restore_snapshot_state(state)?;
                        db.drop_edges_of_retired_nodes();
                    } else if base_is_gone {
                        return Err(GraphError::CommitOutOfRange {
                            commit,
                            total,
                            floor: db.wal_horizon_floor,
                        });
                    }
                }
            } else if !snap_header.is_empty() {
                let snap_bytes = db.fs.read(FileId::Snapshot)?;
                if let Some(state) = core_storage::snapshot::decode(&snap_bytes)? {
                    if state.wal_truncated && !snapshot_is_wal_end {
                        db.restore_snapshot_state(state)?;
                        db.drop_edges_of_retired_nodes();
                    } else if base_is_gone {
                        return Err(GraphError::CommitOutOfRange {
                            commit,
                            total,
                            floor: db.wal_horizon_floor,
                        });
                    }
                }
            }
            // else: snap_header empty = no snapshot file.
            let live_local = local - total_archive_frames;
            for rec in live_records.into_iter().take((live_local + 1) as usize) {
                // The repair a live commit ran before this one (ledger row 111).
                if !is_declaration(&rec) {
                    db.reconcile_lost_provenance();
                }
                db.apply(&rec)?;
                let _ = db.engine.drain_deltas();
            }
        }
        // Pin: pending_delta_count must be 0 after as-of replay, mirroring T1's
        // post-loop assert in open_with.
        debug_assert_eq!(
            db.engine.pending_delta_count(),
            0,
            "pending_deltas non-empty after open_at replay — \
             per-frame drain must run inside the loop to keep memory O(1)"
        );
        let _ = db.engine.drain_deltas(); // belt-and-braces no-op
                                          // Rebuild view values after WAL replay so derived-edge-driven views
                                          // reflect the as-of state.  open_at always uses the legacy path (no V8
                                          // base), so topo_view is always owned.
        {
            let topo_view = TopologyView::owned(&db.topo);
            db.view_store.rebuild_all(
                Arc::make_mut(&mut db.props),
                &topo_view,
                &db.ids,
                &db.syms,
                &db.labels,
            );
        }
        // Rebuild full-text index for as-of view (mirrors open_with pattern).
        Arc::make_mut(&mut db.fulltext).rebuild_all(
            &db.ids,
            &db.labels,
            &db.syms,
            build_props_view(&db.props, &db.base),
        );
        db.drop_indexes_over_view_properties();
        Arc::make_mut(&mut db.prop_index).rebuild_all(
            &db.ids,
            &db.labels,
            &db.syms,
            build_props_view(&db.props, &db.base),
        );
        // Namespaces on the temporal handle, built by the same pass the live
        // open uses, so an as-of mask narrows by the namespaces of that commit.
        db.rebuild_node_ns();
        // Load roles sidecar (current roles, not point-in-time).
        db.roles = Self::load_roles_from_fs(&db.fs)?;
        db.read_only = true;
        db.total_wal_commits = total;
        // Capture initial fold so reader() is immediately usable.
        db.fold_now();
        Ok(db)
    }

    /// Whether this instance is a read-only as-of view.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    // ── MVCC epoch reader ─────────────────────────────────────────────────────

    /// Clone the current overlay state into a new `FrozenOverlay` and reset
    /// the delta tail. Called automatically every `FOLD_EVERY_K` commits and at
    /// the end of `open_with` / `open_at_with` to prime the reader.
    fn fold_now(&mut self) {
        // What the engine's provenance has done since the last fold, each triple
        // netted to its final state. Taken on every fold, so it never holds more
        // than one fold's worth.
        let changes = self.engine.take_derived_changes();
        let derived = match &self.fold_overlay {
            // The previous fold, moved on by what changed since: a fold on the
            // write path, a refresh, a change of roles. Costs the changes, not
            // the size of the store's provenance.
            Some(previous) => previous.derived.extended(changes, &self.base),
            // A load whose engine began as the snapshot base's provenance: that
            // section, read where it lies, and replay's changes over it.
            None if self.engine_from_base && self.base.is_some() => {
                crate::reader::DerivedEdges::from_base().extended(changes, &self.base)
            }
            // Any other load (a store with no snapshot, an as-of view): take
            // the engine's provenance whole.
            None => crate::reader::DerivedEdges::from_triples(self.engine.derived_triples()),
        };
        self.fold_with(derived);
    }

    /// The fold after a snapshot: the engine's provenance is exactly the
    /// section just written, so the reader reads that and nothing is layered.
    fn fold_after_snapshot(&mut self) {
        let _ = self.engine.take_derived_changes();
        let derived = if self.base.is_some() {
            crate::reader::DerivedEdges::from_base()
        } else {
            crate::reader::DerivedEdges::from_triples(self.engine.derived_triples())
        };
        self.fold_with(derived);
    }

    /// The listing the next fold carries: the previous fold's when no
    /// declaration was applied since, else built afresh.
    fn fold_schema(&mut self) -> Arc<SchemaListing> {
        let listing = match &self.fold_overlay {
            Some(previous) if !self.schema_stale => Arc::clone(&previous.schema),
            _ => Arc::new(self.schema_listing()),
        };
        self.schema_stale = false;
        listing
    }

    fn fold_with(&mut self, derived: crate::reader::DerivedEdges) {
        // The fold holds every edge property as it stands, so nothing written
        // so far (replay, a refresh, a commit's own writes) is owed to a delta.
        if self.edge_props.has_touched() {
            Arc::make_mut(&mut self.edge_props).take_touched();
        }
        // Eight `Arc::clone`s — refcount bumps, O(1). This used to deep-copy the
        // whole overlay: `IdMap` alone is a `HashMap<String, u32>` plus a
        // `Vec<String>`, so every node key was copied twice, on every open,
        // after every snapshot, every FOLD_EVERY_K commits on the write path,
        // and — with no commit threshold — on every `refresh()` that applied a
        // peer commit. A refreshing reader now pays nothing for a fold.
        //
        // `props` and `topo` were already cheap for a different reason: on a
        // snapshotted store `restore_v8_base` leaves them as empty overlays over
        // the zero-copy mmap. `ids`, `syms` and `fulltext` were not, and that
        // inconsistency was the defect. The full-text index is no longer
        // shared at all: no reader reads it, and a shared `Arc` made the next
        // full-text write copy it whole under the write guard.
        let frozen = crate::reader::FrozenOverlay {
            ids: std::sync::Arc::clone(&self.ids),
            syms: std::sync::Arc::clone(&self.syms),
            topo: std::sync::Arc::clone(&self.topo),
            props: std::sync::Arc::clone(&self.props),
            labels: std::sync::Arc::clone(&self.labels),
            edge_props: std::sync::Arc::clone(&self.edge_props),
            roles: self.roles.clone().map(std::sync::Arc::new),
            prop_index: std::sync::Arc::clone(&self.prop_index),
            derived,
            schema: self.fold_schema(),
        };
        self.fold_overlay = Some(Arc::new(frozen));
        self.delta_tail.clear();
        // The tail's derived changes moved into the fold's layers.
        self.tail_derived = Arc::new(crate::reader::TailDerived::default());
        self.commits_since_fold = 0;
        // The fold holds every view value as it stands; what replay or a
        // refresh touched on the way here is not owed to any delta.
        self.view_store.clear_touched();
    }

    /// The view properties the commit of `rec` left on the nodes it touched,
    /// for the reader's delta tail — or `None` when the commit should be
    /// folded instead of described.
    ///
    /// Folded: a commit that creates or deletes a view, which writes its
    /// property on every node of a label, and one that moves far more view
    /// values than it has records — a write to a hub moves a view on every
    /// neighbour. A delta listing those would be held, and replayed by every
    /// reader, until the next fold.
    ///
    /// Not folded: a commit whose view values are in proportion to its
    /// records, however many. A bulk load gives each node it inserts a
    /// starting value, and the reader replays each of those inserts anyway. A
    /// fold is eight `Arc` clones to take, but it shares the store's maps
    /// with the reader, so the commit after it copies them in full; a load
    /// that folded on every batch paid that on every batch.
    fn view_values_of_commit(
        &mut self,
        rec: &WalRecord,
    ) -> Option<Vec<(u32, String, Option<Value>)>> {
        /// View values a commit may carry beyond those its own records
        /// account for.
        const MAX_BEYOND_RECORDS: usize = 1024;

        fn declares_a_view(rec: &WalRecord) -> bool {
            match rec {
                WalRecord::CreateView { .. } | WalRecord::DeleteView { .. } => true,
                WalRecord::Batch(inner) => inner.iter().any(declares_a_view),
                _ => false,
            }
        }

        fn leaf_records(rec: &WalRecord) -> usize {
            match rec {
                WalRecord::Batch(inner) => inner.iter().map(leaf_records).sum(),
                _ => 1,
            }
        }

        let mut touched = self.view_store.take_touched();
        if declares_a_view(rec) {
            return None;
        }
        if touched.is_empty() {
            return Some(Vec::new());
        }
        touched.sort_unstable();
        touched.dedup();
        let views = self.view_store.views().count();
        let budget = MAX_BEYOND_RECORDS + leaf_records(rec).saturating_mul(views);
        let cols = build_props_view(&self.props, &self.base);
        let mut values = Vec::new();
        for &node in &touched {
            // A node deleted later in the same commit: the reader's replay of
            // the delete clears its properties, the view's among them.
            if self.ids.key_of(node).is_none() {
                continue;
            }
            let label = self.labels.get(node as usize).copied();
            for def in self.view_store.views() {
                if label.is_none() || self.syms.get(&def.label) != label {
                    continue;
                }
                if values.len() == budget {
                    return None;
                }
                let value = cols.get(node, &def.view_prop).map(|v| v.into_value());
                values.push((node, def.view_prop.clone(), value));
            }
        }
        Some(values)
    }

    /// Capture a lock-free reader snapshot of the current db state.
    ///
    /// The read lock is held only for the duration of this call (to clone a
    /// handful of `Arc` handles). Subsequent query operations run without any
    /// lock.
    pub fn reader(&self) -> crate::reader::ReaderSnapshot {
        self.try_reader()
            .expect("fold_overlay is always Some after open_with; call reader() after open")
    }

    /// [`Self::reader`], or `None` while the handle has no fold (between a
    /// reload's reset and its fold) — when there is no state to publish.
    pub(crate) fn try_reader(&self) -> Option<crate::reader::ReaderSnapshot> {
        let frozen = self.fold_overlay.clone()?;
        let effective = crate::reader::SharedEffective::cell_for(
            &self.shared_effective,
            &frozen,
            &self.base,
            &self.delta_tail,
        );
        let snap = crate::reader::ReaderSnapshot::new(crate::reader::SnapshotParts {
            frozen,
            base: self.base.clone(),
            deltas: Arc::from(self.delta_tail.as_slice()),
            tail_derived: Arc::clone(&self.tail_derived),
            // The snapshot's effective state is exactly this handle's state at
            // this commit, so it shares the memo and its version key.
            version: self.commit_seq,
            role_masks: Arc::clone(&self.role_masks),
            context: crate::reader::ReadContext {
                slow: self.slow_query_sink(),
                counts: (
                    self.ids.live_len(),
                    self.ids.len() - self.ids.live_len(),
                    self.corrected_edge_count(),
                ),
                wal_path: self.fs.wal_path(),
            },
            shared: effective,
        });
        Some(snap)
    }

    /// Append a delta the reader cannot apply, so that a corrupt overlay is
    /// reachable from a test.
    ///
    /// Compiled only under `test-hooks`, which the server's dev-dependency on
    /// this crate turns on. One call permanently corrupts every
    /// [`ReaderSnapshot`](crate::reader::ReaderSnapshot) taken from the handle,
    /// so it must not be in the published surface: `#[doc(hidden)]` hides it
    /// from rustdoc and from nothing else. The feature gate — not
    /// `#[cfg(test)]` — because its only callers are in `crates/server/tests`,
    /// a different crate, exactly as `core_rules`'s index counters are.
    ///
    /// [`ReaderSnapshot::effective`](crate::reader::ReaderSnapshot) lays the
    /// delta tail over the frozen overlay and answers
    /// [`GraphError::Corrupt`] when a record will not apply. Nothing a caller
    /// can do produces that state — `DeltaLayer::apply`'s failures are disagreements
    /// between the tail and the fold it is applied to, which the write path
    /// cannot create — so the `Corrupt` arm of every scoped reader method was
    /// reachable only by inspection until this hook existed. An `Intern` record
    /// claiming an id the frozen interner will not hand back is the smallest
    /// such disagreement.
    ///
    /// Only the tail is touched. This handle's own state is untouched and
    /// `commit_seq` does not move, so a role mask already memoised at this
    /// version stays memoised — which is exactly the state in which the HTTP
    /// role branches reach a scoped read with a corrupt overlay under them.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn push_unapplyable_delta_for_test(&mut self) {
        self.delta_tail.push(Arc::new(crate::reader::CommitDelta {
            records: vec![WalRecord::Intern {
                id: u32::MAX,
                text: "delta-tail-corruption".into(),
            }],
            derived_inserts: Vec::new(),
            derived_deletes: Vec::new(),
            view_values: Vec::new(),
            edge_props: Vec::new(),
        }));
    }

    /// Inject a triple into a second rule's provenance, bypassing `GraphMut::add_edge`'s
    /// base check, and add the edge to the overlay topology so the next snapshot will
    /// double-count it in its header.  Together these reproduce the damaged state that
    /// row 103's fix prevents: two rules each holding the same triple, and an
    /// `edge_count` that is one too high per duplicated edge.  Call `snapshot()` after
    /// this to persist the damage for a row-107 repair test.
    ///
    /// Panics if either key is not live in this store.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn inject_double_ownership_for_test(
        &mut self,
        rule_name: &str,
        edge_type: &str,
        src_key: &str,
        dst_key: &str,
    ) {
        // Load the V8 base sections first: without this the engine's retained
        // provenance bytes are not yet populated, so the mutation call below would
        // find nothing to decode and the injection would be silently discarded.
        self.ensure_v8_base_sections_loaded();
        // Materialize provenance into live fields so the injection is visible to
        // subsequent writes and to the snapshot encoder.
        self.engine.ensure_provenance_loaded_mut();
        let etype = Arc::make_mut(&mut self.syms).intern(edge_type);
        let src = self.ids.get(src_key).unwrap_or_else(|| {
            panic!("inject_double_ownership_for_test: src key {src_key:?} not found")
        });
        let dst = self.ids.get(dst_key).unwrap_or_else(|| {
            panic!("inject_double_ownership_for_test: dst key {dst_key:?} not found")
        });
        self.engine
            .inject_double_ownership_for_test(rule_name, etype, src, dst);
        // Add the edge to the overlay topology so topology_merge_to_csr counts the
        // base copy (from the prior snapshot) plus this overlay copy — making the
        // next snapshot's header one too high per duplicated edge.
        Arc::make_mut(&mut self.topo).add_edge(etype, src, dst);
    }

    /// Whether this handle's state has had the lost-provenance repair (ledger
    /// rows 111, 113), so that a snapshot of it says so.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn lost_provenance_reconciled_for_test(&self) -> bool {
        self.engine.lost_provenance_reconciled()
    }

    /// The raw `edge_count` field from the current V8 snapshot header, without
    /// the `corrected_edge_count` row-walk fix applied.  On a store snapshotted
    /// before the encoder fix (ledger row 107), this returns the inflated value.
    /// Used by tests to assert that new snapshots are written with the correct
    /// header value (proving the encoder fix is needed independently of the
    /// db.rs count fix).
    #[doc(hidden)]
    pub fn snapshot_header_edge_count_for_test(&self) -> Option<u64> {
        self.base
            .as_ref()
            .and_then(|b| b.topology().ok())
            .map(|t| u64::from(t.edge_count))
    }

    /// Total number of WAL commits at the time [`open_at`] was called.
    /// Returns 0 for normal (non-as-of) instances.
    pub fn total_wal_commits(&self) -> u64 {
        self.total_wal_commits
    }

    /// Apply a record to in-memory state. Used by both live writes and replay,
    /// so replay is definitionally identical to the original execution.
    fn apply(&mut self, rec: &WalRecord) -> Result<()> {
        // Before the record mutates anything: a store restored from a snapshot
        // defers building its candidate indexes until the first write, and that
        // build is a full node scan. Left where it used to fire — inside the
        // engine hook, after `props.set` and the label assignment — the scan
        // read the half-applied record and took the in-flight node's vector for
        // one the snapshot should have carried, which read as an interrupted
        // vector-index build and cost a full `RebuildRule` on the first
        // embedded write after every reopen. Hoisted here the scan sees exactly
        // the persisted state; the record's own hook then files its vector
        // through the ordinary insert path a line later.
        //
        // A declaration writes no node and no edge, and its arm reads no rule
        // index, so it does not start the scan.
        if is_declaration(rec) {
            self.schema_stale = true;
        } else {
            self.populate_indexes_before_write();
        }
        // A V11-only record makes the store V11 however it arrives: a live
        // write has already been upgraded at the choke point, so this is for
        // replay — at open, on refresh and in `open_at` — of a WAL that holds
        // one (contract C9). A `Batch` is checked through its inner records.
        if !self.v11_features && !matches!(rec, WalRecord::Batch(_)) && rec_is_v11(rec) {
            self.v11_features = true;
        }
        match rec {
            WalRecord::InsertNode { label, key, props } => {
                let id = Arc::make_mut(&mut self.ids).try_insert(key)?;
                let sym = Arc::make_mut(&mut self.syms).intern(label);
                if self.labels.len() <= id as usize {
                    // gap slots are sentinels, never valid label symbols
                    Arc::make_mut(&mut self.labels).resize(id as usize + 1, u32::MAX);
                }
                Arc::make_mut(&mut self.labels)[id as usize] = sym;
                let mut ns_name = NS_DEFAULT.to_string();
                for (field, value) in props {
                    if field == NS_PROP {
                        ns_name = namespace_of_value(Some(value)).to_string();
                    }
                    Arc::make_mut(&mut self.props).set(id, field, value.clone());
                }
                self.set_node_ns(id, &ns_name);
                // Initialize view values for the new node before the engine runs so
                // delta-based increments start from a known zero baseline.
                self.view_store.init_node_views(
                    id,
                    Arc::make_mut(&mut self.props),
                    &self.syms,
                    &self.labels,
                );
                // Fire rules for the newly inserted node.
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, None, &mut gm);
                }
                self.engine = eng;
                // Process derived-edge deltas for view maintenance.
                // Fast path: skip the O(delta_count) allocation when no views exist.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }
                // Full-text index maintenance: index enabled fields for this label.
                if self.fulltext.has_label(label) {
                    for (field, value) in props {
                        if self.fulltext.is_enabled(label, field) {
                            self.unindex_before_reindexing(id, field);
                            Arc::make_mut(&mut self.fulltext).add_tokens(id, field, value);
                        }
                    }
                }
                // Property (equality) index maintenance.
                if self.prop_index.has_label(label) {
                    for (field, value) in props {
                        Arc::make_mut(&mut self.prop_index).set(label, field, id, value);
                    }
                }
            }
            WalRecord::InsertEdge {
                edge_type,
                src_key,
                dst_key,
            } => {
                let src = self.ids.get(src_key).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("wal replay references unknown key {src_key}"),
                })?;
                let dst = self.ids.get(dst_key).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("wal replay references unknown key {dst_key}"),
                })?;
                let etype = Arc::make_mut(&mut self.syms).intern(edge_type);
                // Skip if the edge is already visible in the merged base+overlay
                // view.  This keeps WAL replay idempotent when the WAL contains
                // pre-snapshot records that are already encoded in a V8 base
                // (keep_wal=true opens and crash-before-truncation scenarios).
                if self.base.is_some()
                    && self
                        .topo_view()
                        .neighbors(etype, Direction::Out, src)
                        .contains(&dst)
                {
                    return Ok(());
                }
                Arc::make_mut(&mut self.topo).add_edge(etype, src, dst);
                // View maintenance for manual edge insert.
                self.view_store.on_edge_changed(
                    etype,
                    src,
                    dst,
                    true,
                    Arc::make_mut(&mut self.props),
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    base_columns(&self.base),
                );
                // Rule engine: via-hop rules must update when user edges change.
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.on_edge_changed(edge_type, src, dst, &mut gm);
                }
                self.engine = eng;
                if !self.view_store.is_empty() {
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }
            }
            WalRecord::SetProp { key, field, value } => {
                let id = self.ids.get(key).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("wal replay references unknown key {key}"),
                })?;
                let old_value = build_props_view(&self.props, &self.base)
                    .get(id, field)
                    .map(|vr| vr.into_value());
                // What the full-text index holds for `id`, so the rewrite
                // below removes that value's words, not the whole vocabulary.
                let indexed = self
                    .fulltext
                    .field_indexed(field)
                    .then(|| old_value.clone())
                    .flatten();
                Arc::make_mut(&mut self.props).set(id, field, value.clone());
                // Fire rules for the changed field.
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, Some((field, old_value)), &mut gm);
                }
                self.engine = eng;
                // Derived-edge deltas → view updates.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }
                // Neighbor-aggregate views that read `field` must also update.
                self.view_store.on_prop_changed(
                    id,
                    field,
                    Arc::make_mut(&mut self.props),
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    base_columns(&self.base),
                );
                // Full-text index maintenance: update tokens for this field if indexed.
                if self.fulltext.field_indexed(field) {
                    let label_opt = self.labels.get(id as usize).and_then(|&sym| {
                        if sym == u32::MAX {
                            None
                        } else {
                            self.syms.resolve(sym)
                        }
                    });
                    if let Some(label) = label_opt {
                        if self.fulltext.is_enabled(label, field) {
                            let fulltext = Arc::make_mut(&mut self.fulltext);
                            fulltext.remove_node_value(id, field, indexed.as_ref());
                            fulltext.add_tokens(id, field, value);
                        }
                    }
                }
                // Property (equality) index maintenance: re-key this node's value.
                if self.prop_index.field_indexed(field) {
                    let label_opt = self.labels.get(id as usize).and_then(|&sym| {
                        if sym == u32::MAX {
                            None
                        } else {
                            self.syms.resolve(sym)
                        }
                    });
                    if let Some(label) = label_opt {
                        Arc::make_mut(&mut self.prop_index).set(label, field, id, value);
                    }
                }
            }
            WalRecord::Intern { id, text } => {
                if let Some(existing) = self.syms.get(text) {
                    if existing != *id {
                        return Err(GraphError::Corrupt {
                            detail: format!(
                                "wal intern mismatch for {text:?}: have {existing}, record {id}"
                            ),
                        });
                    }
                } else {
                    let got = Arc::make_mut(&mut self.syms).intern(text);
                    if got != *id {
                        return Err(GraphError::Corrupt {
                            detail: format!(
                                "wal intern assigned {got} for {text:?}, record wanted {id}"
                            ),
                        });
                    }
                }
            }
            WalRecord::InsertNodeId { label, key, props } => {
                let id = Arc::make_mut(&mut self.ids).try_insert(key)?;
                if self.labels.len() <= id as usize {
                    Arc::make_mut(&mut self.labels).resize(id as usize + 1, u32::MAX);
                }
                Arc::make_mut(&mut self.labels)[id as usize] = *label;
                let label_str = self
                    .syms
                    .resolve(*label)
                    .ok_or_else(|| GraphError::Corrupt {
                        detail: format!("wal InsertNodeId unknown label intern {label}"),
                    })?
                    .to_string();
                let mut ns_name = NS_DEFAULT.to_string();
                for (field_sym, value) in props {
                    let field =
                        self.syms
                            .resolve(*field_sym)
                            .ok_or_else(|| GraphError::Corrupt {
                                detail: format!(
                                    "wal InsertNodeId unknown field intern {field_sym}"
                                ),
                            })?;
                    if field == NS_PROP {
                        ns_name = namespace_of_value(Some(value)).to_string();
                    }
                    Arc::make_mut(&mut self.props).set(id, field, value.clone());
                }
                self.set_node_ns(id, &ns_name);
                self.view_store.init_node_views(
                    id,
                    Arc::make_mut(&mut self.props),
                    &self.syms,
                    &self.labels,
                );
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, None, &mut gm);
                }
                self.engine = eng;
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }
                if self.fulltext.has_label(&label_str) {
                    for (field_sym, value) in props {
                        let Some(field) = self.syms.resolve(*field_sym).map(str::to_string) else {
                            continue;
                        };
                        let field = field.as_str();
                        if self.fulltext.is_enabled(&label_str, field) {
                            self.unindex_before_reindexing(id, field);
                            Arc::make_mut(&mut self.fulltext).add_tokens(id, field, value);
                        }
                    }
                }
                if self.prop_index.has_label(&label_str) {
                    for (field_sym, value) in props {
                        let Some(field) = self.syms.resolve(*field_sym) else {
                            continue;
                        };
                        Arc::make_mut(&mut self.prop_index).set(&label_str, field, id, value);
                    }
                }
            }
            WalRecord::InsertEdgeId { etype, src, dst } => {
                // Replay-over-snapshot: dense ids in the pre-snapshot WAL may
                // already be tombstoned. Skip rather than attaching edges to
                // dead ids (DeleteNode keys the live re-insert, not the old id).
                if self.ids.is_tombstoned(*src)
                    || self.ids.is_tombstoned(*dst)
                    || self.ids.key_of(*src).is_none()
                    || self.ids.key_of(*dst).is_none()
                {
                    return Ok(());
                }
                // Skip if already visible in the merged view (same idempotency
                // guard as InsertEdge above: prevents double-counting when
                // pre-snapshot WAL records are replayed over a V8 base).
                if self.base.is_some()
                    && self
                        .topo_view()
                        .neighbors(*etype, Direction::Out, *src)
                        .contains(dst)
                {
                    return Ok(());
                }
                Arc::make_mut(&mut self.topo).add_edge(*etype, *src, *dst);
                self.view_store.on_edge_changed(
                    *etype,
                    *src,
                    *dst,
                    true,
                    Arc::make_mut(&mut self.props),
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    base_columns(&self.base),
                );
                // Rule engine: via-hop rules fire when user via-edges are inserted.
                // Resolve etype back to string so on_edge_changed can match rules by name.
                if let Some(etype_str) = self.syms.resolve(*etype).map(|s| s.to_string()) {
                    let cursor = self.engine.pending_delta_count();
                    let mut eng = std::mem::take(&mut self.engine);
                    {
                        let mut gm = make_graph_mut(
                            &self.ids,
                            &mut self.syms,
                            &self.labels,
                            build_props_view(&self.props, &self.base),
                            &mut self.topo,
                            &self.base,
                            &mut self.edge_props,
                        );
                        eng.on_edge_changed(&etype_str, *src, *dst, &mut gm);
                    }
                    self.engine = eng;
                    if !self.view_store.is_empty() {
                        let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                        for d in &new_deltas {
                            self.view_store.on_edge_changed(
                                d.etype_sym,
                                d.src_id,
                                d.dst_id,
                                d.fired,
                                Arc::make_mut(&mut self.props),
                                &build_topo_view(&self.topo, &self.base),
                                &self.ids,
                                &self.syms,
                                &self.labels,
                                base_columns(&self.base),
                            );
                        }
                    }
                }
            }
            WalRecord::SetPropId { id, field, value } => {
                if self.ids.is_tombstoned(*id) || self.ids.key_of(*id).is_none() {
                    return Ok(());
                }
                let field_str = self
                    .syms
                    .resolve(*field)
                    .ok_or_else(|| GraphError::Corrupt {
                        detail: format!("wal SetPropId unknown field intern {field}"),
                    })?
                    .to_string();
                let old_value = build_props_view(&self.props, &self.base)
                    .get(*id, &field_str)
                    .map(|vr| vr.into_value());
                // As in `SetProp`: the full-text rewrite removes the old value's words.
                let indexed = self
                    .fulltext
                    .field_indexed(&field_str)
                    .then(|| old_value.clone())
                    .flatten();
                Arc::make_mut(&mut self.props).set(*id, &field_str, value.clone());
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(*id, Some((field_str.as_str(), old_value)), &mut gm);
                }
                self.engine = eng;
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }
                self.view_store.on_prop_changed(
                    *id,
                    &field_str,
                    Arc::make_mut(&mut self.props),
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    base_columns(&self.base),
                );
                if self.fulltext.field_indexed(&field_str) {
                    let label_opt = self.labels.get(*id as usize).and_then(|&sym| {
                        if sym == u32::MAX {
                            None
                        } else {
                            self.syms.resolve(sym)
                        }
                    });
                    if let Some(label) = label_opt {
                        if self.fulltext.is_enabled(label, &field_str) {
                            let fulltext = Arc::make_mut(&mut self.fulltext);
                            fulltext.remove_node_value(*id, &field_str, indexed.as_ref());
                            fulltext.add_tokens(*id, &field_str, value);
                        }
                    }
                }
                if self.prop_index.field_indexed(&field_str) {
                    let label_opt = self.labels.get(*id as usize).and_then(|&sym| {
                        if sym == u32::MAX {
                            None
                        } else {
                            self.syms.resolve(sym)
                        }
                    });
                    if let Some(label) = label_opt {
                        Arc::make_mut(&mut self.prop_index).set(label, &field_str, *id, value);
                    }
                }
            }
            WalRecord::CreateRule { def_bytes } => {
                let def: RuleDef = decode_rule_def(def_bytes).map_err(|e| GraphError::Corrupt {
                    detail: format!("CreateRule def_bytes deserialize failed: {e}"),
                })?;
                // Replay-over-snapshot idempotency: the rule was captured in the snapshot
                // so the engine already has it; silently skip to avoid a spurious
                // RuleInvalid error in the crash window between snapshot write and WAL
                // truncation.
                if self.engine.rules().any(|r| r.name == def.name) {
                    return Ok(());
                }
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.create_rule(def, &mut gm)
                };
                self.engine = eng;
                result.map_err(|e| GraphError::RuleInvalid { detail: e })?;
                // Derived-edge fires from backfill → view updates.
                // Fast path: skip O(edge_count) allocation when no views exist.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }
            }
            WalRecord::DeleteRule { name } => {
                // Replay-over-snapshot idempotency: the snapshot already captured the
                // post-delete state so the rule is absent; silently skip to avoid a
                // spurious RuleNotFound error in the crash window between snapshot write
                // and WAL truncation.
                if !self.engine.rules().any(|r| r.name == *name) {
                    return Ok(());
                }
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.delete_rule(name, &mut gm)
                };
                self.engine = eng;
                result.map_err(|_| GraphError::RuleNotFound { name: name.clone() })?;
                // Derived-edge retractions → view updates.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }
            }
            WalRecord::RemoveProp { key, field } => {
                // Recovery-safe: unknown key or already-absent field is a
                // clean no-op. Crash-window replay over a snapshot that
                // already applied this record must not Err.
                let Some(id) = self.ids.get(key) else {
                    return Ok(());
                };
                // Read old value through the seam for rule retraction.
                let old = build_props_view(&self.props, &self.base)
                    .get(id, field)
                    .map(|vr| vr.into_value());
                // As in `SetProp`: the full-text removal walks the old value's words.
                let indexed = self
                    .fulltext
                    .field_indexed(field)
                    .then(|| old.clone())
                    .flatten();
                Arc::make_mut(&mut self.props).remove(id, field);
                // If the base still supplies the value after the overlay removal,
                // record a tombstone so ColumnsView::get does not resurrect it.
                // This covers both the base-only case AND the both-resident case:
                //   base-only (in_overlay=false): old prop was only in base, remove
                //     is a no-op on overlay, base still visible → tombstone needed.
                //   both-resident (in_overlay=true): overlay had v2, base has v1;
                //     removing overlay uncovers v1 → tombstone needed.
                // Idempotent on double-replay: second pass sees the tombstone →
                // get() returns None → condition is false → no duplicate tombstone.
                if build_props_view(&self.props, &self.base)
                    .get(id, field)
                    .is_some()
                {
                    Arc::make_mut(&mut self.props).record_prop_tombstone(id, field);
                }
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, Some((field, old)), &mut gm);
                }
                self.engine = eng;
                // Derived-edge deltas → view updates.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }
                // Neighbor-aggregate views that read `field` must also update.
                self.view_store.on_prop_changed(
                    id,
                    field,
                    Arc::make_mut(&mut self.props),
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    base_columns(&self.base),
                );
                // Full-text index maintenance: remove tokens for this field.
                if self.fulltext.field_indexed(field) {
                    Arc::make_mut(&mut self.fulltext).remove_node_value(
                        id,
                        field,
                        indexed.as_ref(),
                    );
                }
                // Property (equality) index maintenance: drop this node's entry.
                if self.prop_index.field_indexed(field) {
                    if let Some(label) = self.labels.get(id as usize).and_then(|&sym| {
                        (sym != u32::MAX).then(|| self.syms.resolve(sym)).flatten()
                    }) {
                        Arc::make_mut(&mut self.prop_index).remove_node(label, field, id);
                    }
                }
            }
            WalRecord::DeleteEdge {
                edge_type,
                src_key,
                dst_key,
            } => {
                // Recovery-safe: unknown keys, unknown etype, or already-
                // absent edge is a clean no-op (remove_edge returns false).
                let Some(src) = self.ids.get(src_key) else {
                    return Ok(());
                };
                let Some(dst) = self.ids.get(dst_key) else {
                    return Ok(());
                };
                let Some(etype) = self.syms.get(edge_type) else {
                    return Ok(());
                };
                // I3: phantom-tombstone guard.  When a V8 base is present, a
                // DeleteEdge WAL record for an edge that was already absorbed into
                // the new base (i.e. neither in overlay nor in base) must be skipped.
                // Without this guard, remove_edge records a tombstone for an edge
                // that no longer exists, incorrectly understating edge_count.
                if self.base.is_some()
                    && !self
                        .topo_view()
                        .neighbors(etype, core_storage::topology::Direction::Out, src)
                        .contains(&dst)
                {
                    return Ok(());
                }
                Arc::make_mut(&mut self.topo).remove_edge(etype, src, dst);
                Arc::make_mut(&mut self.edge_props).remove_edge(etype, src, dst);
                // View maintenance for manual edge delete (topo already updated above).
                self.view_store.on_edge_changed(
                    etype,
                    src,
                    dst,
                    false,
                    Arc::make_mut(&mut self.props),
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    base_columns(&self.base),
                );
                // Rule engine: via-hop rules must retract when user via-edges are deleted.
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.on_edge_changed(edge_type, src, dst, &mut gm);
                }
                self.engine = eng;
                if !self.view_store.is_empty() {
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }
            }
            WalRecord::DeleteNode { key } => {
                // Recovery-safe: already-tombstoned / unknown key is a clean
                // no-op. Crash-window replay over a snapshot that already
                // applied this record cannot recover the retired id from the
                // key (`IdMap::get` is None), so every subsequent step is
                // skipped. Each step is independently idempotent if invoked
                // twice on a still-live id: retraction is a no-op on empty
                // provenance, `remove_edge` returns false, `remove_all` is a
                // no-op, `ids.delete` returns None, label sentinel is sticky.
                let Some(n) = self.ids.get(key) else {
                    return Ok(());
                };

                // (1) Retract derived edges + de-index while props/labels live.
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.on_node_removed(n, &mut gm);
                }
                self.engine = eng;
                // Derived-edge retractions → view updates for neighbors.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }

                // (2) Sweep ALL remaining edges incident to n, both directions,
                // every etype. This cascade is intentionally mask-independent:
                // topology integrity requires removing every edge touching the
                // deleted node regardless of the caller's visibility scope.
                // (The mask limits which nodes a role's read phase can return;
                // the WAL delete always executes with full storage authority.)
                // Collect then remove so neighbor slices stay valid during
                // iteration. Remove from topo first, then call view maintenance
                // so Avg/Min/Max recompute sees the correct (reduced) neighbor set.
                //
                // Through the view of the snapshot and what was written since,
                // not `self.topo` alone: that is only what was written since,
                // and an edge the snapshot holds was left behind, pointing at
                // a node that was gone (ledger row 95).
                let mut doomed = Vec::new();
                {
                    let tv = self.topo_view();
                    let etypes: Vec<u32> = tv.etypes().collect();
                    for et in &etypes {
                        for &dst in tv.neighbors(*et, Direction::Out, n).as_ref() {
                            doomed.push((*et, n, dst));
                        }
                        for &src in tv.neighbors(*et, Direction::In, n).as_ref() {
                            // A self-loop is already listed from its Out side.
                            if src != n {
                                doomed.push((*et, src, n));
                            }
                        }
                    }
                }
                for (et, s, d) in doomed {
                    Arc::make_mut(&mut self.topo).remove_edge(et, s, d);
                    Arc::make_mut(&mut self.edge_props).remove_edge(et, s, d);
                    // View maintenance: n's own view values will be cleared by
                    // remove_all below; only update surviving neighbors.
                    self.view_store.on_edge_changed_except(
                        et,
                        s,
                        d,
                        false,
                        Arc::make_mut(&mut self.props),
                        &build_topo_view(&self.topo, &self.base),
                        &self.ids,
                        &self.syms,
                        &self.labels,
                        base_columns(&self.base),
                        Some(n),
                    );
                }

                // (3) Drop every remaining prop (`ColumnStore::remove_all`).
                Arc::make_mut(&mut self.props).remove_all(n);
                // Full-text index maintenance: remove all tokens for this node.
                Arc::make_mut(&mut self.fulltext).remove_node(n);
                // Property (equality) index maintenance: drop all entries for n.
                Arc::make_mut(&mut self.prop_index).remove_node_all(n);

                // (4) Retire the dense id and stamp the label sentinel.
                Arc::make_mut(&mut self.ids).delete(key);
                if let Some(slot) = Arc::make_mut(&mut self.labels).get_mut(n as usize) {
                    *slot = u32::MAX;
                }
            }
            WalRecord::Batch(inner) => {
                // Apply each inner record in order through the same apply path.
                // Inner records are validated free of nested Batch by encode_record.
                for rec in inner {
                    self.apply(rec)?;
                }
            }
            WalRecord::RebuildRule { name } => {
                // Replay-over-snapshot idempotency: the snapshot may already
                // reflect a later delete_rule, so the rule is absent; skip.
                if !self.engine.rules().any(|r| r.name == *name) {
                    return Ok(());
                }
                let cursor = self.engine.pending_delta_count();
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        build_props_view(&self.props, &self.base),
                        &mut self.topo,
                        &self.base,
                        &mut self.edge_props,
                    );
                    eng.rebuild(name, &mut gm)
                };
                self.engine = eng;
                result.map_err(|_| GraphError::RuleNotFound { name: name.clone() })?;
                // Derived-edge delta changes → view updates.
                if !self.view_store.is_empty() {
                    #[cfg(test)]
                    DELTA_COPY_COUNT.with(|c| c.set(c.get() + 1));
                    let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
                    for d in &new_deltas {
                        self.view_store.on_edge_changed(
                            d.etype_sym,
                            d.src_id,
                            d.dst_id,
                            d.fired,
                            Arc::make_mut(&mut self.props),
                            &build_topo_view(&self.topo, &self.base),
                            &self.ids,
                            &self.syms,
                            &self.labels,
                            base_columns(&self.base),
                        );
                    }
                }
            }
            WalRecord::CreateView { def_bytes } => {
                let def: ViewDef =
                    bincode::deserialize(def_bytes).map_err(|e| GraphError::Corrupt {
                        detail: format!("CreateView def_bytes deserialize failed: {e}"),
                    })?;
                // Replay-over-snapshot idempotency: view already present → skip.
                if self.view_store.has_view(&def.name) {
                    return Ok(());
                }
                self.view_store
                    .create_view_over(
                        def,
                        Arc::make_mut(&mut self.props),
                        &build_topo_view(&self.topo, &self.base),
                        &self.ids,
                        &self.syms,
                        &self.labels,
                        base_columns(&self.base),
                    )
                    .map_err(|e| GraphError::RuleInvalid { detail: e })?;
            }
            WalRecord::DeleteView { name } => {
                // Replay-over-snapshot idempotency: view already absent → skip.
                if !self.view_store.has_view(name) {
                    return Ok(());
                }
                let doomed_prop = self
                    .view_store
                    .views()
                    .find(|v| v.name == *name)
                    .map(|v| (v.label.clone(), v.view_prop.clone()));
                self.view_store
                    .delete_view(
                        name,
                        Arc::make_mut(&mut self.props),
                        &self.ids,
                        &self.labels,
                        &self.syms,
                    )
                    .map_err(|_| GraphError::RuleNotFound { name: name.clone() })?;
                // `delete_view` clears the view's property from what was
                // written since the snapshot. A value the snapshot holds is
                // hidden by a tombstone, or it stays readable on every node
                // of the label after the view that owned it is gone.
                if let (Some((label, prop)), true) = (doomed_prop, self.base.is_some()) {
                    if let Some(label_sym) = self.syms.get(&label) {
                        for id in 0..self.ids.len() as u32 {
                            if self.labels.get(id as usize).copied() == Some(label_sym)
                                && build_props_view(&self.props, &self.base)
                                    .get(id, &prop)
                                    .is_some()
                            {
                                Arc::make_mut(&mut self.props).record_prop_tombstone(id, &prop);
                            }
                        }
                    }
                }
            }
            WalRecord::EnableFulltext { label, field } => {
                // Replay-over-snapshot idempotency: already enabled → skip.
                if self.fulltext.is_enabled(label, field) {
                    return Ok(());
                }
                Arc::make_mut(&mut self.fulltext).enable(label, field);
                if self.fulltext_rebuild_follows {
                    // The open path rebuilds the whole index after replay, which
                    // clears every posting this scan would write. Doing it twice
                    // costs a full tokenise-and-stem pass over the corpus per
                    // enabled pair: measured at 456 ms against 3.8 ms for the
                    // same 30,000-entity store with no pair enabled.
                    return Ok(());
                }
                // Backfill: index all live nodes of this label that have the field.
                let n = self.ids.len() as u32;
                for id in 0..n {
                    let Some(&sym) = self.labels.get(id as usize) else {
                        continue;
                    };
                    if sym == u32::MAX {
                        continue; // tombstoned
                    }
                    let Some(lbl) = self.syms.resolve(sym) else {
                        continue;
                    };
                    if lbl != label {
                        continue;
                    }
                    if let Some(value) = build_props_view(&self.props, &self.base)
                        .get(id, field)
                        .map(|vr| vr.into_value())
                    {
                        Arc::make_mut(&mut self.fulltext).add_tokens(id, field, &value);
                    }
                }
            }
            WalRecord::DisableFulltext { label, field } => {
                // Replay-over-snapshot idempotency: already disabled → skip.
                if !self.fulltext.is_enabled(label, field) {
                    return Ok(());
                }
                // If another label still indexes this field, the postings column
                // is kept — but it must not contain node_ids from the now-disabled
                // label.  Remove them before calling disable() so the field_indexed
                // guard inside disable() sees the correct post-removal state.
                if self.fulltext.field_indexed_by_other(label, field) {
                    if let Some(label_sym) = self.syms.get(label) {
                        for (node_id, &lsym) in self.labels.iter().enumerate() {
                            if lsym == label_sym {
                                Arc::make_mut(&mut self.fulltext)
                                    .remove_node_field(node_id as u32, field);
                            }
                        }
                    }
                }
                Arc::make_mut(&mut self.fulltext).disable(label, field);
            }
            WalRecord::EnableIndex { label, field } => {
                // Replay-over-snapshot idempotency: already declared → skip.
                if self.prop_index.is_equality_declared(label, field) {
                    return Ok(());
                }
                // A pair a range index maintains already has its postings.
                let maintained = self.prop_index.is_enabled(label, field);
                Arc::make_mut(&mut self.prop_index).enable(label, field);
                if !maintained {
                    self.backfill_prop_index(label, field);
                }
            }
            WalRecord::DisableIndex { label, field } => {
                Arc::make_mut(&mut self.prop_index).disable(label, field);
            }
            // A declaration: the postings come from the backfill here on a live
            // or refreshed handle, and from `rebuild_all` at the end of an open.
            WalRecord::EnableRangeIndex { name, label, field } => {
                // Replay-over-snapshot idempotency: already declared → skip.
                if self.prop_index.range_index(name).is_some() {
                    return Ok(());
                }
                let maintained = self.prop_index.is_enabled(label, field);
                Arc::make_mut(&mut self.prop_index).enable_range(name, label, field);
                // The open rebuilds every declared pair once replay is done, so
                // a scan here would be thrown away.
                if !maintained && !self.replaying_at_open {
                    self.backfill_prop_index(label, field);
                }
            }
            WalRecord::DisableRangeIndex { name } => {
                Arc::make_mut(&mut self.prop_index).disable_range(name);
            }
            // A declaration only: the data it covers is never checked here.
            // Replay must not fail on duplicates written before the constraint
            // was declared; the check is for new writes, at the dense rewrite.
            WalRecord::CreateConstraint { name, label, field } => {
                // Replay-over-snapshot idempotency: already declared → skip.
                if self.constraints.iter().any(|c| &c.name == name) {
                    return Ok(());
                }
                // The backing index comes with the declaration. It is an
                // equality declaration even over a range-indexed pair, so that
                // dropping the range index leaves the constraint its index.
                if !self.prop_index.is_equality_declared(label, field) {
                    self.apply(&WalRecord::EnableIndex {
                        label: label.clone(),
                        field: field.clone(),
                    })?;
                }
                self.constraints.push(UniqueConstraint {
                    name: name.clone(),
                    label: label.clone(),
                    field: field.clone(),
                });
            }
            WalRecord::DropConstraint { name } => {
                self.constraints.retain(|c| &c.name != name);
            }
            // ── insert-count multiplicity (§5.13) ────────────────────────────
            //
            // Two shapes, told apart by `count`: the opt-in declaration, and an
            // absolute count for one triple. Absolute is what makes this
            // idempotent over a snapshot base — a pre-snapshot frame replayed
            // over a base that already folded it in lands on the same number
            // rather than adding to it, which is the failure a delta (or a count
            // derived from `InsertEdgeId` records) would have.
            WalRecord::SetEdgeCount {
                etype,
                src,
                dst,
                count,
            } => {
                if rec.is_multiplicity_decl() {
                    self.multiplicity = true;
                } else {
                    Arc::make_mut(&mut self.edge_props).set(
                        *etype,
                        *src,
                        *dst,
                        EDGE_COUNT_PROP,
                        Value::Int(*count as i64),
                    );
                }
            }
            // History markers carry no replay state — rules re-derive edges
            // deterministically on open/replay. Skip unconditionally.
            WalRecord::DerivedEdgeAdded { .. } | WalRecord::DerivedEdgeRetracted { .. } => {}
            // ── rename_node ──────────────────────────────────────────────────
            WalRecord::RenameNode { old_key, new_key } => {
                // Recovery-safe: if old_key is already gone (key was renamed
                // by a snapshot or a prior replay frame), skip cleanly.
                if self.ids.get(old_key).is_none() {
                    return Ok(());
                }
                // The rename only updates the key-table; the dense id, all
                // topo edges, props, labels, and rule state are id-indexed and
                // require no change.
                Arc::make_mut(&mut self.ids)
                    .rename(old_key, new_key)
                    .map_err(|e| GraphError::Corrupt {
                        detail: format!("wal replay RenameNode {old_key}→{new_key}: {e}"),
                    })?;
            }
        }
        Ok(())
    }

    /// Index every live node of `label` that holds `field` under the declared
    /// pair `(label, field)`: the scan a new declaration needs on a handle
    /// whose open has already rebuilt its indexes.
    fn backfill_prop_index(&mut self, label: &str, field: &str) {
        let n = self.ids.len() as u32;
        for id in 0..n {
            let Some(&sym) = self.labels.get(id as usize) else {
                continue;
            };
            if sym == u32::MAX {
                continue; // tombstoned
            }
            let Some(lbl) = self.syms.resolve(sym) else {
                continue;
            };
            if lbl != label {
                continue;
            }
            if let Some(value) = build_props_view(&self.props, &self.base)
                .get(id, field)
                .map(|vr| vr.into_value())
            {
                Arc::make_mut(&mut self.prop_index).set(label, field, id, &value);
            }
        }
    }

    /// Intern `s` in `syms` and emit a WAL `Intern` record so `*Id` records
    /// replay on WAL-only `open_at` (no snapshot intern table). Apply is
    /// idempotent when the string is already bound. Always emit: after
    /// `snapshot()` the WAL is truncated and live intern is not on disk.
    fn intern_wal(&mut self, s: &str) -> (u32, WalRecord) {
        let id = if let Some(id) = self.syms.get(s) {
            id
        } else {
            Arc::make_mut(&mut self.syms).intern(s)
        };
        (
            id,
            WalRecord::Intern {
                id,
                text: s.to_string(),
            },
        )
    }

    /// Rewrite user-facing records into dense-id records. On `Err`, no live
    /// state is left mutated: speculative interns made while building the
    /// output are rolled back, so a later successful mutation cannot log an
    /// `Intern` record whose id replay would never reproduce.
    fn rewrite_wal_dense(&mut self, recs: Vec<WalRecord>) -> Result<Vec<WalRecord>> {
        self.rewrite_wal_dense_planned(recs.into_iter().map(PlannedRec::Rec).collect())
    }

    /// [`rewrite_wal_dense`](Self::rewrite_wal_dense) for a frame that still
    /// carries [`PlannedRec::DuplicateCount`] entries — the shape a batch
    /// produces, where a duplicate's count can only be named once this pass has
    /// assigned the frame's own ids.
    ///
    /// This is the one function every value-carrying WAL record passes through
    /// on its way to the WAL — `insert_node` and `set_prop` via `log_dense`,
    /// every batch, ingest and Cypher write via `commit_logged_batch` — so it
    /// is where the store moves to the V11 format (contract C9): the first
    /// frame carrying a V11-only record (see [`rec_is_v11`]) writes a V11
    /// snapshot, keeping the WAL, before anything is interned or appended. A
    /// 0.9.x reader then stops at `snapshot: unsupported version 11` and
    /// never reaches the frame it could not decode. Replay never comes here.
    ///
    /// The upgrade runs before the frame's other checks that need the dense
    /// ids — a unique-constraint violation found after it still refuses the
    /// write, and leaves the store V11 with nothing V11 written. That is the
    /// safe direction: the stamp costs an older reader a refusal, never a
    /// commit.
    ///
    /// It is also where unique constraints are enforced (0.10 TY-5), for the
    /// same reason: it sees every frame a write produces, whole, after its keys
    /// have been resolved to the dense ids the frame will give them. A
    /// duplicate — against committed state or inside the frame — refuses the
    /// whole frame before anything is appended.
    fn rewrite_wal_dense_planned(&mut self, recs: Vec<PlannedRec>) -> Result<Vec<WalRecord>> {
        let mut carries_v11 = false;
        for rec in &recs {
            if let PlannedRec::Rec(rec) = rec {
                if let Some((field, size)) = oversized_binary(rec, &self.syms) {
                    return Err(GraphError::ValueTooLarge {
                        field,
                        size,
                        limit: BINARY_MAX_BYTES,
                    });
                }
                if let Some(offset_seconds) = out_of_range_offset(rec) {
                    return Err(GraphError::QueryError {
                        detail: format!(
                            "DateTime offset of {offset_seconds} s is outside ±18:00; a \
                             DateTime cannot be stored with it"
                        ),
                    });
                }
                carries_v11 = carries_v11 || (!self.v11_features && rec_is_v11(rec));
            }
        }
        if carries_v11 {
            if self.in_nosync_group {
                // Unreachable through `commit_group` and the drain, which
                // upgrade ahead of the group; refused rather than snapshot a
                // state the group has not made durable.
                return Err(GraphError::QueryError {
                    detail: "the V11 format upgrade cannot run inside a group commit; the \
                             group did not upgrade ahead of its first write"
                        .into(),
                });
            }
            self.upgrade_to_v11()?;
        }
        let syms_checkpoint = self.syms.len();
        let result = self
            .rewrite_wal_dense_inner(recs)
            .and_then(|out| self.check_unique_constraints(&out).map(|()| out));
        if result.is_err() {
            Arc::make_mut(&mut self.syms).truncate(syms_checkpoint);
        }
        result
    }

    fn rewrite_wal_dense_inner(&mut self, recs: Vec<PlannedRec>) -> Result<Vec<WalRecord>> {
        let mut out = Vec::with_capacity(recs.len());
        // Node ids allocated by later apply(InsertNodeId) in this same batch.
        let mut pending: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        // Namespace of each node inserted earlier in this same frame, so a SET
        // on a node this frame created is measured against the namespace it was
        // created in rather than against the store, where it does not exist yet.
        let mut pending_ns: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // Whether each node this frame has inserted, renamed or set `id` on has
        // `id == key` at that point of the frame. The store alone cannot say:
        // it does not have the frame's own writes.
        let mut pending_id_mirrors_key: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();
        let mut interned = std::collections::HashSet::<u32>::new();
        let mut next = u32::try_from(self.ids.len()).map_err(|_| GraphError::Corrupt {
            detail: "id space exhausted".into(),
        })?;
        // Insert counts this frame has already raised. `edge_insert_count`
        // reads committed state, which cannot see a count queued earlier in
        // this same frame, so N duplicates of one pair would otherwise all
        // compute `committed + 1` and the last would win.
        let mut pending_counts: HashMap<(u32, u32, u32), u64> = HashMap::new();
        // Keys this frame has taken off the node the store holds them on: the
        // old key of a rename, and a deleted node's. Until the frame inserts
        // under such a key again it names nothing, and after that it names the
        // new node — never the stored one, which is what the store alone
        // would still answer while the frame is being rewritten.
        let mut freed: std::collections::HashSet<String> = std::collections::HashSet::new();
        let lookup = |ids: &IdMap,
                      pending: &std::collections::HashMap<String, u32>,
                      freed: &std::collections::HashSet<String>,
                      key: &str|
         -> Option<u32> {
            match pending.get(key) {
                Some(&id) => Some(id),
                None if freed.contains(key) => None,
                None => ids.get(key),
            }
        };
        for rec in recs {
            // A duplicate insert's count, resolved here and nowhere else.
            //
            // This is the only pass that knows the frame's own ids: a node
            // created earlier in the same frame has no dense id until the
            // `InsertNodeId` above allocates one, and an edge type first used in
            // this frame is not in `syms` until `intern_wal` puts it there.
            // Resolving the count in the batch's validate pass instead — where
            // it used to live — meant that a duplicate whose endpoints or type
            // were created in the same frame silently produced no count at all,
            // which is exactly the shape a mirror rebuild writes (defect #24).
            let rec = match rec {
                PlannedRec::Rec(rec) => rec,
                PlannedRec::DuplicateCount {
                    edge_type,
                    src_key,
                    dst_key,
                } => {
                    let (etype, intern) = self.intern_wal(&edge_type);
                    if interned.insert(etype) {
                        out.push(intern);
                    }
                    let src = lookup(&self.ids, &pending, &freed, &src_key).ok_or_else(|| {
                        GraphError::Corrupt {
                            detail: format!("dense WAL rewrite missing src {src_key}"),
                        }
                    })?;
                    let dst = lookup(&self.ids, &pending, &freed, &dst_key).ok_or_else(|| {
                        GraphError::Corrupt {
                            detail: format!("dense WAL rewrite missing dst {dst_key}"),
                        }
                    })?;
                    let count = pending_counts
                        .get(&(etype, src, dst))
                        .copied()
                        .unwrap_or_else(|| self.edge_insert_count(etype, src, dst))
                        .saturating_add(1);
                    pending_counts.insert((etype, src, dst), count);
                    out.push(WalRecord::SetEdgeCount {
                        etype,
                        src,
                        dst,
                        count,
                    });
                    continue;
                }
            };
            match rec {
                WalRecord::InsertNode { label, key, props } => {
                    for (field, value) in &props {
                        refuse_null_property(&key, field, value)?;
                    }
                    pending_id_mirrors_key.insert(
                        key.clone(),
                        props
                            .iter()
                            .any(|(f, v)| f == "id" && id_mirrors_key(&key, Some(v))),
                    );
                    // Namespace validation and normalisation, on the one seam
                    // every user-visible node insert passes through: insert_node,
                    // a batch, ingest, Cypher CREATE and MERGE all arrive here
                    // before the WAL append, and replay never does.
                    let (props, ns_name) = Self::normalise_insert_ns(&key, props)?;
                    pending_ns.insert(key.clone(), ns_name);
                    let (label_id, intern) = self.intern_wal(&label);
                    if interned.insert(label_id) {
                        out.push(intern);
                    }
                    let mut props_id = Vec::with_capacity(props.len());
                    for (field, value) in props {
                        let (field_id, intern) = self.intern_wal(&field);
                        if interned.insert(field_id) {
                            out.push(intern);
                        }
                        props_id.push((field_id, value));
                    }
                    if lookup(&self.ids, &pending, &freed, &key).is_none() {
                        pending.insert(key.clone(), next);
                        next = next.checked_add(1).ok_or_else(|| GraphError::Corrupt {
                            detail: "id space exhausted".into(),
                        })?;
                    }
                    out.push(WalRecord::InsertNodeId {
                        label: label_id,
                        key,
                        props: props_id,
                    });
                }
                WalRecord::SetProp { key, field, value } => {
                    refuse_null_property(&key, &field, &value)?;
                    // `id` mirrors the key: a write may restate the key, never
                    // change it (ledger row 128). Rename moves it with the key.
                    if field == "id" {
                        let mirrors = match pending_id_mirrors_key.get(&key) {
                            Some(&m) => m,
                            None => id_mirrors_key(&key, self.get_prop(&key, "id").as_ref()),
                        };
                        if mirrors && !id_mirrors_key(&key, Some(&value)) {
                            return Err(key_property_error(&key));
                        }
                        pending_id_mirrors_key
                            .insert(key.clone(), id_mirrors_key(&key, Some(&value)));
                    }
                    // A namespace is set at insert and fixed after: the write is
                    // refused when it would move the node, and dropped when it
                    // names the namespace the node is already in. Checked here
                    // so set_prop, a batch, Cypher SET/MERGE and every upsert
                    // that merges props get the same answer.
                    if field == NS_PROP {
                        let Value::Str(ref to) = value else {
                            return Err(GraphError::RuleInvalid {
                                detail: format!(
                                    "node {key}: {NS_PROP} must be a string naming a namespace, \
                                     got {value:?}"
                                ),
                            });
                        };
                        let from = pending_ns
                            .get(&key)
                            .cloned()
                            .or_else(|| self.namespace_of(&key))
                            .unwrap_or_else(|| NS_DEFAULT.to_string());
                        let to = to.clone();
                        if to != from {
                            return Err(GraphError::NamespaceImmutable {
                                key: key.clone(),
                                from,
                                to,
                            });
                        }
                        continue;
                    }
                    let id = lookup(&self.ids, &pending, &freed, &key).ok_or_else(|| {
                        GraphError::Corrupt {
                            detail: format!("dense WAL rewrite missing key {key}"),
                        }
                    })?;
                    let (field_id, intern) = self.intern_wal(&field);
                    if interned.insert(field_id) {
                        out.push(intern);
                    }
                    out.push(WalRecord::SetPropId {
                        id,
                        field: field_id,
                        value,
                    });
                }
                WalRecord::InsertEdge {
                    edge_type,
                    src_key,
                    dst_key,
                } => {
                    let (etype, intern) = self.intern_wal(&edge_type);
                    if interned.insert(etype) {
                        out.push(intern);
                    }
                    let src = lookup(&self.ids, &pending, &freed, &src_key).ok_or_else(|| {
                        GraphError::Corrupt {
                            detail: format!("dense WAL rewrite missing src {src_key}"),
                        }
                    })?;
                    let dst = lookup(&self.ids, &pending, &freed, &dst_key).ok_or_else(|| {
                        GraphError::Corrupt {
                            detail: format!("dense WAL rewrite missing dst {dst_key}"),
                        }
                    })?;
                    out.push(WalRecord::InsertEdgeId { etype, src, dst });
                }
                WalRecord::RenameNode {
                    ref old_key,
                    ref new_key,
                } => {
                    // Track the rename in `pending` so subsequent InsertEdge /
                    // SetProp records in this batch can resolve the new key.
                    let id = lookup(&self.ids, &pending, &freed, old_key).ok_or_else(|| {
                        GraphError::Corrupt {
                            detail: format!(
                                "dense WAL rewrite: RenameNode old key {old_key} not found"
                            ),
                        }
                    })?;
                    pending.remove(old_key.as_str());
                    freed.insert(old_key.clone());
                    pending.insert(new_key.clone(), id);
                    // The node keeps its `id` value, which was the old key at
                    // most, so it no longer mirrors the new key.
                    pending_id_mirrors_key.remove(old_key.as_str());
                    pending_id_mirrors_key.insert(new_key.clone(), false);
                    out.push(rec);
                }
                // # Symbol-order invariant (load-bearing)
                //
                // Write-time and replay-time symbol assignment must agree: every
                // symbol in a `Batch` frame has to receive the same dense id when
                // the frame's records are replayed in order as it received when
                // the frame was written.
                //
                // A rule's backfill interns its `edge_type` lazily
                // (`core_rules::engine`, every `g.syms.intern(&def.edge_type)`
                // site), and that backfill runs from `apply` — during the
                // `CreateRule` record itself, and again from any later
                // `InsertNodeId` in the same frame that makes the rule fire. At
                // write time the whole batch is rewritten before any of it is
                // applied, so a later `InsertEdge` in the same batch would win the
                // lower id for its edge type; on replay the rule's lazy intern
                // gets there first and steals it, and the `Intern` record fails at
                // the `wal intern assigned …` check in `apply`.
                //
                // Pre-interning the rule's `edge_type` here, and emitting its
                // `Intern` record ahead of the `CreateRule` record, makes both
                // orders identical. `weight_prop` needs no pre-intern:
                // `EdgeProps::set` keys props by `String`, never through the
                // interner. `via_edge` needs none either: via-hop rules resolve it
                // with `syms.get` and skip when it is absent.
                //
                // `RebuildRule` and `DeleteRule` need no such handling here:
                // `RebuildRule` has no `BatchOp` variant, so it never appears
                // inside a `Batch` today — it is only ever issued as its own
                // standalone commit (`rebuild_rule`, or the auto-rebuild path
                // that logs it as a second commit after the triggering op).
                // `DeleteRule` does have a `BatchOp` variant and can appear
                // inside a `Batch`, but it carries only a rule `name` — no
                // `edge_type` or other symbol that needs pre-interning — so
                // only `CreateRule` needs this arm.
                WalRecord::CreateRule { ref def_bytes } => {
                    let def = decode_rule_def(def_bytes).map_err(|e| GraphError::Corrupt {
                        detail: format!("CreateRule def_bytes deserialize failed: {e}"),
                    })?;
                    let (etype, intern) = self.intern_wal(&def.edge_type);
                    if interned.insert(etype) {
                        out.push(intern);
                    }
                    out.push(rec);
                }
                WalRecord::DeleteNode { ref key } => {
                    pending_id_mirrors_key.remove(key.as_str());
                    pending.remove(key.as_str());
                    freed.insert(key.clone());
                    out.push(rec);
                }
                other => out.push(other),
            }
        }
        Ok(out)
    }

    /// Refuse a frame that would leave two live nodes of a constrained label
    /// holding equal values for the constrained field.
    ///
    /// `out` is the frame as [`rewrite_wal_dense_inner`](Self::rewrite_wal_dense_inner)
    /// produced it. Keys are resolved here exactly as that pass resolved them —
    /// a node the frame inserts takes the next free id, and a deleted or
    /// renamed-away key names nothing until the frame inserts it again — and
    /// every label and field the frame names is already interned. What is
    /// judged is the state after the frame: each node the frame touches by its
    /// final label and value, every other node by what the equality index
    /// holds. So a frame that swaps two values, or frees a value and takes it
    /// in one commit, passes; one that writes a value twice, or a value an
    /// untouched node holds, does not.
    ///
    /// The error names the constraint and the field only — never the node
    /// already holding the value, nor whether the caller could see it — so it
    /// reads the same for a visible node, a hidden one and another node in the
    /// same write (spec §4.6).
    fn check_unique_constraints(&self, out: &[WalRecord]) -> Result<()> {
        if self.constraints.is_empty() {
            return Ok(());
        }
        // Every field the frame names is interned by now, so a constrained
        // field with no symbol is one the frame cannot write.
        let field_syms: Vec<Option<u32>> = self
            .constraints
            .iter()
            .map(|c| self.syms.get(&c.field))
            .collect();
        let constrained = |sym: u32| field_syms.contains(&Some(sym));
        let constrained_name = |name: &str| self.constraints.iter().any(|c| c.field == name);

        let mut pending: HashMap<&str, u32> = HashMap::new();
        let mut freed: HashSet<&str> = HashSet::new();
        let mut next = self.ids.len() as u32;
        let lookup = |pending: &HashMap<&str, u32>, freed: &HashSet<&str>, key: &str| match pending
            .get(key)
        {
            Some(&id) => Some(id),
            None if freed.contains(key) => None,
            None => self.ids.get(key),
        };
        // What the frame leaves behind, for the nodes it touches.
        let mut touched: BTreeSet<u32> = BTreeSet::new();
        let mut dead: HashSet<u32> = HashSet::new();
        let mut final_label: HashMap<u32, u32> = HashMap::new();
        let mut final_value: HashMap<(u32, u32), Option<&Value>> = HashMap::new();
        for rec in out {
            match rec {
                WalRecord::InsertNodeId { label, key, props } => {
                    let id = match lookup(&pending, &freed, key) {
                        Some(id) => id,
                        None => {
                            pending.insert(key.as_str(), next);
                            next = next.saturating_add(1);
                            next - 1
                        }
                    };
                    touched.insert(id);
                    final_label.insert(id, *label);
                    for (field, value) in props {
                        if constrained(*field) {
                            final_value.insert((id, *field), Some(value));
                        }
                    }
                }
                WalRecord::SetPropId { id, field, value } if constrained(*field) => {
                    touched.insert(*id);
                    final_value.insert((*id, *field), Some(value));
                }
                WalRecord::RemoveProp { key, field } if constrained_name(field) => {
                    if let (Some(id), Some(sym)) =
                        (lookup(&pending, &freed, key), self.syms.get(field))
                    {
                        touched.insert(id);
                        final_value.insert((id, sym), None);
                    }
                }
                WalRecord::DeleteNode { key } => {
                    if let Some(id) = lookup(&pending, &freed, key) {
                        touched.insert(id);
                        dead.insert(id);
                    }
                    pending.remove(key.as_str());
                    freed.insert(key.as_str());
                }
                WalRecord::RenameNode { old_key, new_key } => {
                    if let Some(id) = lookup(&pending, &freed, old_key) {
                        pending.remove(old_key.as_str());
                        freed.insert(old_key.as_str());
                        pending.insert(new_key.as_str(), id);
                    }
                }
                _ => {}
            }
        }

        let committed = self.ids.len();
        let props_view = build_props_view(&self.props, &self.base);
        // What earlier nodes of this frame hold, per constraint, in the
        // bucket `equality_bucket` gives; values in one bucket are compared
        // with `values_equal`, the scan's own equality.
        let mut claimed: HashMap<(usize, EqualityBucket), Vec<Value>> = HashMap::new();
        for &id in &touched {
            if dead.contains(&id) {
                continue;
            }
            let label = final_label
                .get(&id)
                .or_else(|| self.labels.get(id as usize))
                .copied()
                .filter(|&sym| sym != u32::MAX)
                .and_then(|sym| self.syms.resolve(sym));
            let Some(label) = label else {
                continue;
            };
            // A node the frame moves into a constrained label brings the
            // value it holds with it; that is a claim like any other.
            let relabelled = (id as usize) < committed
                && final_label
                    .get(&id)
                    .is_some_and(|sym| self.labels.get(id as usize) != Some(sym));
            for (ci, c) in self.constraints.iter().enumerate() {
                if c.label != label {
                    continue;
                }
                let stored = || {
                    if (id as usize) < committed {
                        props_view.get(id, &c.field).map(|vr| vr.into_value())
                    } else {
                        None
                    }
                };
                let written = field_syms[ci].and_then(|sym| final_value.get(&(id, sym)));
                let value = match written {
                    Some(Some(v)) => {
                        // Only a value the frame changes is judged. One it
                        // leaves as it was — a rewrite of the same value, or a
                        // change reverted inside the frame — is not a claim,
                        // so a duplicate that already exists is not refused
                        // for being written again.
                        if !relabelled && stored().is_some_and(|old| values_equal(&old, v)) {
                            continue;
                        }
                        (*v).clone()
                    }
                    Some(None) => continue,
                    // Not written: the node keeps what it holds, and judging
                    // that is only owed when it has just arrived in the label.
                    None if relabelled => match stored() {
                        Some(v) => v,
                        None => continue,
                    },
                    None => continue,
                };
                // A value no scan can find equal to another — one with no
                // index key, or a NaN — never conflicts.
                let Some(bucket) = equality_bucket(&value) else {
                    continue;
                };
                let violation = || GraphError::ConstraintViolation {
                    constraint: c.name.clone(),
                    field: c.field.clone(),
                };
                // A holder keeps its value unless the frame deletes it or
                // writes the field to something else. The index answers as
                // the scan does: an `Int` finds the equal `Float`, and the
                // reverse.
                let leaves = |holder: u32| {
                    dead.contains(&holder)
                        || field_syms[ci]
                            .and_then(|sym| final_value.get(&(holder, sym)))
                            .is_some_and(|now| match now {
                                Some(v) => !values_equal(v, &value),
                                None => true,
                            })
                };
                if self
                    .prop_index
                    .lookup(&c.label, &c.field, &value)
                    .into_iter()
                    .any(|holder| holder != id && !leaves(holder))
                {
                    return Err(violation());
                }
                let held = claimed.entry((ci, bucket)).or_default();
                if held.iter().any(|other| values_equal(other, &value)) {
                    return Err(violation());
                }
                held.push(value);
            }
        }
        Ok(())
    }

    fn log_dense(&mut self, recs: Vec<WalRecord>) -> Result<()> {
        let recs = self.rewrite_wal_dense(recs)?;
        match recs.len() {
            0 => Ok(()),
            1 => self.log_then_apply(recs.into_iter().next().unwrap()),
            _ => self.log_then_apply(WalRecord::Batch(recs)),
        }
    }

    /// Durable write, then notify the event sink. Replay (`apply` during
    /// `open`) never enters this function, so it is the replay-silent seam.
    /// Record that the commit occupying `frame_index` happened now, and append
    /// those 16 bytes to the sidecar.
    ///
    /// `frame_index` is the **global 0-based WAL frame index** of the commit's
    /// own record — the space every history surface addresses — taken from
    /// [`wal_frames_written`](GraphDb::wal_frames_written) before the append
    /// that puts the record there.
    ///
    /// **It is deliberately not derived from `commit_seq`.** A commit is not a
    /// frame: one whose rules fire appends a second frame for the derived-edge
    /// history marker, so `commit_seq - 1` falls one frame further behind per
    /// rule-firing commit and every date resolves to an ever-earlier graph.
    /// That was the shipped behaviour through v0.6.11 and it failed silently,
    /// because an older graph is a plausible answer rather than an error.
    ///
    /// Called from exactly one place — `log_then_apply_with`, immediately after
    /// `commit_seq` is incremented. Every write path in the engine funnels
    /// through that function, and replay deliberately does not: `apply_frames`
    /// re-applies commits that already happened, so stamping there would record
    /// replay time as commit time.
    ///
    /// **This is the engine's only wall-clock read.** Everything else uses
    /// `Instant`, which is monotonic and not a date.
    ///
    /// Failure is swallowed on purpose. The sidecar is not part of the WAL or
    /// the snapshot, so a failed append must not fail a commit that is already
    /// durable — it costs a date, not data. The map is marked poisoned so the
    /// gap is reported rather than resolved across.
    fn stamp_commit_time(&mut self, frame_index: u64) {
        if self.commit_times_poisoned {
            return;
        }
        let unix_ms = match self.commit_time_override {
            Some(ms) => ms,
            None => {
                let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                else {
                    // A clock before 1970. Refuse to invent a timestamp.
                    self.commit_times_poisoned = true;
                    return;
                };
                now.as_millis() as i64
            }
        };
        let first = self.commit_times.is_empty();
        self.commit_times.push(frame_index, unix_ms);

        let wrote = if first {
            self.fs.write_atomic(
                FileId::CommitTimes,
                &core_storage::commit_times::encode(&self.commit_times),
            )
        } else {
            self.fs.append(
                FileId::CommitTimes,
                &core_storage::commit_times::encode_entry(frame_index, unix_ms),
            )
        };
        if wrote.is_err() {
            self.commit_times_poisoned = true;
        }
    }

    /// Read the time sidecar from disk into this handle.
    ///
    /// Absent is the normal case for any store written before v0.6.11 and is
    /// not an error; unreadable is recorded so date queries can say "damaged"
    /// rather than "none recorded".
    ///
    /// Called at open **and** by `refresh` when a peer's frames are absorbed.
    /// Both, because the map is a file another process appends to: a handle
    /// that raises its frame cursor to include a peer's commits while holding a
    /// stale map would answer dates from a prefix of the truth — and, if its
    /// own map were still empty, would rewrite the whole file with one entry
    /// and destroy the peer's.
    fn load_commit_times_from_fs(&mut self) {
        match self.fs.read(FileId::CommitTimes) {
            Ok(bytes) if bytes.is_empty() => {}
            Ok(bytes) => {
                // A map from an older format version is discarded, not
                // reported as damage and not reinterpreted. Its entries were
                // written correctly against a different meaning of the number
                // — see `COMMIT_TIMES_VERSION` — and reading them in this
                // build's space would resolve dates onto unrelated commits.
                // Leaving the map empty makes the store answer
                // `NoRecordedTime`, which is the truth: it records no times
                // this build can use, and the next commit starts a usable map.
                if core_storage::commit_times::superseded_version(&bytes).is_some() {
                    return;
                }
                match core_storage::commit_times::decode(&bytes) {
                    Ok(t) => self.commit_times = t,
                    Err(_) => self.commit_times_poisoned = true,
                }
            }
            Err(_) => {}
        }
    }

    /// Rewrite the sidecar from memory. Used after truncation, which is the one
    /// operation that cannot be expressed as an append.
    fn rewrite_commit_times(&mut self) {
        if self.commit_times_poisoned {
            return;
        }
        if self
            .fs
            .write_atomic(
                FileId::CommitTimes,
                &core_storage::commit_times::encode(&self.commit_times),
            )
            .is_err()
        {
            self.commit_times_poisoned = true;
        }
    }

    /// The greatest commit whose recorded time is at or before `unix_ms`.
    ///
    /// Errors name what they can answer instead of guessing a commit:
    /// `Corrupt` when the sidecar would not decode, `NoRecordedTime` when the
    /// store records none, `TimeBeforeFloor` when the instant predates the
    /// oldest entry, and `CommitOutOfRange` when the answer falls below the WAL
    /// horizon and so cannot be replayed.
    ///
    /// The answer is a **0-based frame index**, ready to hand to `edges_at` or
    /// `was_linked` without adjustment.
    pub fn resolve_instant(&self, unix_ms: i64) -> Result<u64> {
        if self.commit_times_poisoned {
            return Err(GraphError::Corrupt {
                detail: "commit_times.bin will not decode; date queries are \
                         unavailable on this store"
                    .into(),
            });
        }
        let at = self
            .commit_times
            .resolve_instant(unix_ms, self.wal_horizon_floor)?;
        // The map outlives the history it describes. A truncating snapshot folds
        // the WAL and discards it, so entries can name commits the engine can no
        // longer replay — the floor check above catches pruning, and this catches
        // discarding. Returning an index the caller's next call will reject is a
        // two-step error where one will do, and `resolve_date` is public: it
        // either hands back a usable index or refuses.
        let total = self.wal_total_commits()?;
        if at >= total {
            return Err(GraphError::CommitOutOfRange {
                commit: at,
                total,
                floor: self.wal_horizon_floor,
            });
        }
        Ok(at)
    }

    /// Record subsequent commits as having happened at `unix_ms`, or pass
    /// `None` to go back to the system clock.
    ///
    /// For **backfilled history**: a mirror importing rows that already carry
    /// their own timestamps, or a replay of events that happened months ago.
    /// Without this every such commit is stamped "now", so a store holding a
    /// year of imported history answers every date question with
    /// `TimeBeforeFloor` — the data is there and no date reaches it.
    ///
    /// Sticky until changed or cleared, because a day of backfilled rows
    /// genuinely shares one instant.
    ///
    /// **Import in chronological order.** A supplied instant earlier than
    /// anything already recorded is refused with
    /// [`GraphError::CommitTimeNotMonotonic`], because resolution walks commit
    /// order: a later commit carrying an earlier time would silently widen every
    /// answer after it. Equal is allowed — that is what a shared day means. The
    /// live clock is never held to this, so an NTP step backwards still commits.
    ///
    /// Deliberately **not** exposed over HTTP or MCP: asserting when a commit
    /// happened rewrites the store's apparent history, which is not something a
    /// role token models. It is an embedding-caller's operation.
    pub fn record_commits_at(&mut self, unix_ms: Option<i64>) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if let Some(ms) = unix_ms {
            if self.commit_times_poisoned {
                return Err(GraphError::Corrupt {
                    detail: "commit_times.bin will not decode; this store cannot \
                             record an asserted commit time"
                        .into(),
                });
            }
            if let Some(newest) = self.commit_times.max_ms() {
                if ms < newest {
                    return Err(GraphError::CommitTimeNotMonotonic {
                        supplied_ms: ms,
                        newest_ms: newest,
                    });
                }
            }
        }
        self.commit_time_override = unix_ms;
        Ok(())
    }

    /// The instant subsequent commits are being recorded at, when one is set.
    pub fn commit_time_override(&self) -> Option<i64> {
        self.commit_time_override
    }

    /// [`Self::edges_at`] addressed by an instant rather than a commit index.
    ///
    /// Resolves through [`Self::resolve_instant`] — the last commit at or
    /// before the instant — then answers exactly as the commit-indexed call
    /// does. A store that records no times refuses by name; it never guesses.
    pub fn edges_at_instant(&self, key: &str, unix_ms: i64) -> Result<Vec<EdgeAt>> {
        let commit = self.resolve_instant(unix_ms)?;
        self.edges_at(key, commit)
    }

    /// [`Self::was_linked`] addressed by an instant rather than a commit index.
    pub fn was_linked_at_instant(
        &self,
        a: &str,
        b: &str,
        edge_type: &str,
        unix_ms: i64,
    ) -> Result<bool> {
        let commit = self.resolve_instant(unix_ms)?;
        self.was_linked(a, b, edge_type, commit)
    }

    /// Parse an RFC 3339 instant (or a bare `YYYY-MM-DD`) and resolve it.
    ///
    /// The one place every caller-facing surface converts a date string, so
    /// HTTP, MCP, Python and the CLI cannot drift in what they accept.
    pub fn resolve_date(&self, s: &str) -> Result<u64> {
        // The **end** of what the string denotes. A bare date is a day, so it
        // resolves to the last commit at or before that day's end — resolving to
        // the midnight that starts it would exclude everything that happened on
        // the date the caller asked about.
        let ms = core_storage::commit_times::parse_rfc3339_end_ms(s).ok_or_else(|| {
            GraphError::QueryError {
                detail: format!(
                    "could not parse {s:?} as a date; expected RFC 3339 \
                     (2026-06-19, or 2026-06-19T12:00:00Z)"
                ),
            }
        })?;
        self.resolve_instant(ms)
    }

    /// The recorded wall-clock time of `commit`, when the sidecar holds one.
    ///
    /// `commit` is a **0-based frame index** — the space `edges_at`,
    /// `was_linked` and the history events use, not the 1-based `commit_seq`.
    pub fn commit_time_ms(&self, commit: u64) -> Option<i64> {
        if self.commit_times_poisoned {
            return None;
        }
        self.commit_times.time_of(commit)
    }

    /// Read the snapshot's provenance into the engine, for a check or report
    /// that reads which rule owns an edge before the commit does. On the first
    /// write after a clean open, `ensure_provenance_loaded_mut` alone finds no
    /// bytes to decode, because the snapshot's sections are not read yet, and
    /// the store looks as if no rule owns anything (ledger rows 108 and 110).
    fn load_provenance_for_write(&mut self) {
        self.ensure_v8_base_sections_loaded();
        self.engine.ensure_provenance_loaded_mut();
    }

    /// Run the engine's one repair of a snapshot whose provenance counts edges
    /// its graph does not hold (ledger row 111), with the view maintenance the
    /// edges it gives back call for. Called before the first commit that is
    /// not a declaration — live, in WAL replay and in an as-of handle's replay
    /// — so every handle repeats the repair at the same point; a no-op after
    /// the first call.
    fn reconcile_lost_provenance(&mut self) {
        let cursor = self.engine.pending_delta_count();
        let mut eng = std::mem::take(&mut self.engine);
        {
            let mut gm = make_graph_mut(
                &self.ids,
                &mut self.syms,
                &self.labels,
                build_props_view(&self.props, &self.base),
                &mut self.topo,
                &self.base,
                &mut self.edge_props,
            );
            eng.reconcile_lost_provenance(&mut gm);
        }
        self.engine = eng;
        if !self.view_store.is_empty() {
            let new_deltas: Vec<_> = self.engine.pending_deltas_since(cursor).to_vec();
            for d in &new_deltas {
                self.view_store.on_edge_changed(
                    d.etype_sym,
                    d.src_id,
                    d.dst_id,
                    d.fired,
                    Arc::make_mut(&mut self.props),
                    &build_topo_view(&self.topo, &self.base),
                    &self.ids,
                    &self.syms,
                    &self.labels,
                    base_columns(&self.base),
                );
            }
        }
    }

    /// Ensure provenance is loaded and the one-time lost-provenance repair has
    /// run, then drain the engine's pending deltas so the next commit satisfies
    /// invariant I-1 (zero pending deltas at entry).
    ///
    /// Call this before reading derived-edge counts for a report when the first
    /// non-declaration commit has not yet run — for example, before comparing
    /// the edge set either side of a `remove_prop` that is itself the first
    /// write on a damaged store.  Without it, the repair runs *inside* the
    /// commit, making a "before" snapshot taken before the commit see no edges
    /// (the store looks empty before reconcile) while the "after" snapshot sees
    /// zero as well (the property is gone), and the diff reports nothing removed
    /// (ledger row 115).
    ///
    /// Safe to call multiple times: `reconcile_lost_provenance` is a no-op on
    /// its second and subsequent calls, and `drain_deltas` returns nothing
    /// when no deltas are pending.  Does not write to the WAL.
    pub fn ensure_reconcile_for_report(&mut self) {
        self.load_provenance_for_write();
        self.reconcile_lost_provenance();
        // Drain the deltas reconcile produced.  The view store was already
        // notified by reconcile_lost_provenance; the deltas themselves are
        // transient bookkeeping consumed at the next log_then_apply_with call.
        // Draining here satisfies I-1 for any commit that follows.
        let _ = self.engine.drain_deltas();
    }

    fn log_then_apply(&mut self, rec: WalRecord) -> Result<()> {
        self.log_then_apply_with(rec, None, self.fsync)
    }

    /// Whether this frame must fsync under `policy`.
    ///
    /// Batched contract: user-visible batches (>1 mutation) fsync; single
    /// mutations do not. The dense rewrite wraps a single mutation in a
    /// `Batch([Intern.., <one *Id record>])`, so `Intern` records are excluded
    /// from the count — removing that filter would make every single-op write
    /// fsync under Batched (or, if the threshold were raised instead, skip a
    /// needed fsync for real two-op batches).
    fn wal_needs_sync(policy: FsyncPolicy, rec: &WalRecord) -> bool {
        match policy {
            FsyncPolicy::Relaxed => false,
            FsyncPolicy::Strict => true,
            FsyncPolicy::Batched => match rec {
                // Intern + one mutation is the single-op rewrite, not a user batch.
                WalRecord::Batch(inner) => {
                    inner
                        .iter()
                        .filter(|r| !matches!(r, WalRecord::Intern { .. }))
                        .count()
                        > 1
                }
                _ => false,
            },
        }
    }

    /// # Apply-infallibility invariant (load-bearing)
    ///
    /// The ordering is: WAL append → fsync → apply. If `apply` returned `Err`
    /// for a `Batch` frame after a successful WAL write, the WAL would contain
    /// the full frame while in-memory state would reflect only the ops before
    /// the failure. On reopen, WAL replay would then apply the entire batch —
    /// diverging permanently from what the pre-crash process had in memory.
    ///
    /// For `Batch` frames this situation cannot arise because:
    /// - All validation runs via `commit_logged_batch`/`MutPreview` **before**
    ///   the WAL write. `MutPreview` uses the same `&mut self` that apply will
    ///   use, with no concurrent mutation between validation exit and apply entry.
    /// - Every `apply` arm for a validated op is either infallible by construction
    ///   (`InsertNode`, `RemoveProp`, `DeleteEdge`, `DeleteNode`), has idempotency
    ///   guards that return `Ok(())` (`CreateRule`, `DeleteRule`), or is
    ///   guaranteed-present by validation (`InsertEdge`/`SetProp` key lookups).
    /// - `on_node_changed` and `on_node_removed` return `()` — never `Err`.
    ///
    /// A `debug_assert!` below fires in debug builds if `apply` ever returns
    /// `Err` for a `Batch` frame, making any future regression immediately visible
    /// in tests rather than silently diverging crash-recovery behaviour.
    fn log_then_apply_with(
        &mut self,
        rec: WalRecord,
        ingest: Option<(String, usize)>,
        policy: FsyncPolicy,
    ) -> Result<()> {
        // Read-only guard: as-of instances must never write the WAL.
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // Degraded guard: fsync failure left WAL truncated, or a refresh failed
        // partway; in-memory state is ahead of (or out of step with) the
        // on-disk WAL, so further mutations would deepen the divergence.
        // Reopen the database to recover.  Checked before the lock guard: this
        // is the more serious condition and the more useful error.
        if self.degraded {
            return Err(GraphError::Io(std::io::Error::other(
                "database degraded after group-commit fsync failure; reopen required",
            )));
        }
        // Cross-process guard: this write scope asked for the store's write
        // lock and did not get it. Writing anyway would append frames on top of
        // a WAL another process is extending, so refuse instead.
        if self.lock_denied {
            return Err(self.lock_denied_error());
        }
        // Ensure retained provenance bytes are decoded into the live mutable
        // fields before any mutation touches self.engine.provenance.  This is a
        // no-op if provenance was never stored (fresh store) or has already been
        // consumed (subsequent mutations).  WAL replay calls apply() directly
        // and is covered by consume_retained_state_eager before replay.
        self.ensure_v8_base_sections_loaded();
        self.engine.ensure_provenance_loaded_mut();
        // Invariant (I-1): no stale deltas may enter from a previous apply.
        // If any engine method ever accumulates deltas before erroring, they would
        // contaminate the *next* commit's event stream. This assert fires in debug
        // builds, making any future regression visible at the earliest point.
        debug_assert_eq!(
            self.engine.pending_delta_count(),
            0,
            "stale engine deltas at log_then_apply_with entry — \
             a previous apply arm may have accumulated deltas before erroring; \
             the caller must drain_deltas() on any error path before returning"
        );
        if self.intent_unsettled {
            self.settle_snapshot_intent()?;
        }
        // Backstop for the upgrade in `rewrite_wal_dense_planned`: a path that
        // reached the WAL without passing through it would show up here.
        debug_assert!(
            self.v11_features || !rec_is_v11(&rec),
            "a format-V11 value or record reached the WAL append without the V11 upgrade"
        );
        // Fallible only for a record that carries a null property value, which
        // `rewrite_wal_dense` has already refused; nothing is appended then.
        let frame = try_encode_record(&rec)?;
        self.fs.append(FileId::Wal, &frame)?;
        // The first append to a store with no WAL creates the file: learn
        // which one it is. One stat, once (where a file identity exists).
        if cfg!(unix) && self.wal_ident.is_none() {
            self.wal_ident = self.fs.wal_ident().map_err(GraphError::Io)?;
            self.durable_wal
                .snapshot_is(self.snapshot_ident, self.wal_ident);
        }
        // The cursor advances by exactly the bytes appended: these frames are
        // ours and already applied, so a later refresh must not replay them.
        self.wal_consumed += frame.len() as u64;
        self.wal_frames_written += 1;
        if Self::wal_needs_sync(policy, &rec) {
            self.fs.sync(FileId::Wal)?;
            self.note_wal_synced();
        }
        // Marker writing always needs the engine deltas, but the engine only
        // accumulates them when emit_deltas is true (normally gated on subscribers
        // or views being present).  Enable emission for this apply if it is
        // currently off, then restore the original state unconditionally via an
        // RAII guard — this prevents a panic in apply() from leaking the flag.
        // The same guard resets the engine's transient chaining state. A panic
        // unwinding out of a rule hook would otherwise leave `chain_depth`
        // non-zero, which makes every later `begin_chain` decide chaining is
        // already running and silently switch it off for good.
        struct RestoreEmitDeltas(*mut RuleEngine, bool);
        impl Drop for RestoreEmitDeltas {
            fn drop(&mut self) {
                // SAFETY: pointer into self (GraphDb); guard is dropped within
                // this frame before log_then_apply_with returns.
                unsafe {
                    (*self.0).set_emit_deltas(self.1);
                    (*self.0).reset_chain_state();
                }
            }
        }
        let original_emit = self.engine.emit_deltas();
        if !original_emit {
            self.engine.set_emit_deltas(true);
        }
        // SAFETY: raw pointer into self; guard dropped within this frame.
        let _emit_guard = RestoreEmitDeltas(&mut self.engine as *mut _, original_emit);

        if !is_declaration(&rec) {
            self.reconcile_lost_provenance();
        }
        // Capture the delta cursor after reconcile so we can measure apply's
        // own retractions below (ledger row 50).
        let pre_apply_delta = self.engine.pending_delta_count();
        let apply_result = self.apply(&rec);
        // For Batch frames, post-validation apply must be infallible (see above).
        // A debug_assert here catches any future change that makes apply fallible
        // before the caller notices via silent WAL/memory divergence.
        if matches!(&rec, WalRecord::Batch(_)) {
            debug_assert!(
                apply_result.is_ok(),
                "Batch apply returned Err after successful WAL write — \
                 the validate-then-apply invariant has been violated; \
                 see log_then_apply_with invariant doc"
            );
        }
        if apply_result.is_err() {
            // Discard any partial deltas accumulated by the failed apply.
            // They must not ride the next commit's event stream (I-1).
            // _emit_guard restores emit_deltas on drop automatically.
            let _ = self.engine.drain_deltas();
            let _ = self.engine.take_rebuild_needed();
            self.last_apply_retraction_count = 0;
            apply_result?;
        }
        self.commit_seq += 1;
        let seq = self.commit_seq;
        // Update per-node last-change map for the committed record.
        // Must happen after commit_seq is incremented so the seq is correct.
        self.update_last_change_from_rec(&rec, seq);
        // Drain engine deltas and distribute to subscribers before the existing
        // MutationEvent sink fires — both happen post-fsync, post-apply.
        // _emit_guard restores emit_deltas after this line when it drops.
        let engine_deltas = self.engine.drain_deltas();
        // Count retractions from the apply only (not reconcile's restorations).
        // `delete_node` uses this to count via-hop derived edges that
        // `provenance_touching` cannot see because the via node is not a src or
        // dst of the derived edge (ledger row 50).
        self.last_apply_retraction_count = engine_deltas
            .get(pre_apply_delta..)
            .map(|s| s.iter().filter(|d| !d.fired).count() as u64)
            .unwrap_or(0);

        // Append history-marker WAL records for any derived-edge changes so
        // that `edge_history` and `was_linked` can surface rule-attributed
        // events. Markers are STATE NO-OPS during replay; they are written
        // without an additional fsync (the triggering commit's sync already
        // happened; the next commit's sync covers these lazily).
        if !engine_deltas.is_empty() {
            let markers: Vec<WalRecord> = engine_deltas
                .iter()
                .map(|d| {
                    if d.fired {
                        WalRecord::DerivedEdgeAdded {
                            rule: d.rule.clone(),
                            edge_type: d.edge_type.clone(),
                            src_key: d.src_key.clone(),
                            dst_key: d.dst_key.clone(),
                        }
                    } else {
                        WalRecord::DerivedEdgeRetracted {
                            rule: d.rule.clone(),
                            edge_type: d.edge_type.clone(),
                            src_key: d.src_key.clone(),
                            dst_key: d.dst_key.clone(),
                        }
                    }
                })
                .collect();
            let marker_frame = if markers.len() == 1 {
                markers.into_iter().next().unwrap()
            } else {
                WalRecord::Batch(markers)
            };
            // Ignore append errors: markers are best-effort history
            // annotations. Losing them does not affect state correctness.
            // The cursor only advances when the bytes actually landed.
            let marker_bytes = encode_record(&marker_frame);
            if self.fs.append(FileId::Wal, &marker_bytes).is_ok() {
                self.wal_consumed += marker_bytes.len() as u64;
                // A marker is a state no-op during replay but it is not an
                // index no-op: it occupies a frame that every history surface
                // counts. Missing this increment is the whole of defect 1.
                self.wal_frames_written += 1;
            }
        }

        // Stamp the commit against the **last** frame it wrote.
        //
        // `edges_at` and `was_linked` reconstruct derived edges by reading the
        // history markers out of the WAL, not by re-running rules over a
        // prefix. A commit's complete state — its record *and* the edges its
        // rules derived — is therefore only reached at its marker frame, so
        // that is the frame a date naming this commit must resolve to. Stamping
        // the record's own frame would answer every date with the graph as it
        // was one derivation short.
        //
        // This runs after the marker append for that reason, and it is still
        // the single stamping site: one commit, one entry.
        self.stamp_commit_time(self.wal_frames_written - 1);

        // Record MVCC CommitDelta for the epoch reader.  The WAL record is
        // stored as-is (including any nested Batch / Intern records); the
        // reader's `DeltaLayer::apply` handles all variants.
        {
            // Each triple in the state the commit left it. The engine reports a
            // retraction and a re-derivation of one edge as two deltas, in
            // order; reading them as two lists lost that order and made the
            // edge disappear (or stay) wrongly.
            let mut net: std::collections::HashMap<(u32, u32, u32), bool> =
                std::collections::HashMap::new();
            for d in &engine_deltas {
                net.insert((d.etype_sym, d.src_id, d.dst_id), d.fired);
            }
            let derived_inserts = net
                .iter()
                .filter(|(_, &fired)| fired)
                .map(|(&triple, _)| triple)
                .collect();
            let derived_deletes = net
                .iter()
                .filter(|(_, &fired)| !fired)
                .map(|(&triple, _)| triple)
                .collect();
            // Only when there is something to add: the published snapshot
            // shares the tail's index, so touching it copies it.
            if !net.is_empty() {
                Arc::make_mut(&mut self.tail_derived)
                    .append(&crate::reader::DerivedChanges::from_net(&net));
            }
            // `None`: the commit moved more view values than a delta should
            // carry, so the fold below takes them instead.
            let view_values = self.view_values_of_commit(&rec);
            // A commit that declares or drops an index or a constraint folds
            // too: a snapshot answers indexed lookups from the fold's index,
            // and `SHOW` from the fold's listing, so both must be the store's
            // (see `ReaderSnapshot::query_index`).
            let fold = view_values.is_none() || declares_schema(&rec);
            // Every edge whose properties this commit wrote — a rule's weights
            // above all, which no record carries — as the commit left it.
            let edge_props = if self.edge_props.has_touched() {
                let touched = Arc::make_mut(&mut self.edge_props).take_touched();
                touched
                    .into_iter()
                    .map(|(et, s, d)| ((et, s, d), self.edge_props.entry(et, s, d)))
                    .collect()
            } else {
                Vec::new()
            };
            let delta = Arc::new(crate::reader::CommitDelta {
                records: vec![rec.clone()],
                derived_inserts,
                derived_deletes,
                view_values: view_values.unwrap_or_default(),
                edge_props,
            });
            self.delta_tail.push(delta);
            self.commits_since_fold += 1;
            if fold || self.commits_since_fold >= crate::reader::FOLD_EVERY_K {
                self.fold_now();
            }
        }

        if self.defer_events {
            // Group-commit drain thread: hold events until after the group
            // fsync so subscribers only observe durable data (R2).
            self.deferred_events.push(DeferredEvent {
                rec: rec.clone(),
                engine_deltas,
                seq,
                ingest,
            });
        } else {
            self.distribute_events(&rec, &engine_deltas, seq);
            self.emit_committed(&rec, ingest);
        }
        // Drift is only known after apply, so auto-rebuild cannot join the
        // triggering op's WAL frame. Issue RebuildRule as a second commit.
        // Skip when `rec` is itself RebuildRule: rebuild resets drift, so a
        // retrigger loop is impossible if the fit succeeded, but we still
        // drain the flag so a leftover cannot re-enter.
        // One slice of any outstanding vector-index build rides here too, so a
        // store that is being written to finishes its build without anyone
        // calling `pump_index_build`. A rule that becomes whole joins the same
        // RebuildRule loop below.
        let mut rebuilds = self.engine.take_rebuild_needed();
        if !matches!(&rec, WalRecord::RebuildRule { .. }) {
            // Not after `CreateRule`: that record's own apply already did the
            // rule's first slice, and pumping again here would make one
            // `create_rule` call do two slices' work under one lock.
            // Nothing pending is the overwhelmingly common case and must cost
            // a map lookup, not an engine swap: a store being written to has
            // long since populated its indexes, so the `pump_index_build`
            // entry point owns the not-yet-populated case on its own.
            if !is_create_rule_frame(&rec) && !self.engine.builds_in_progress().is_empty() {
                rebuilds.extend(self.pump_one_slice().into_iter().map(|b| b.rule));
            }
            let mut failed = Vec::new();
            for name in rebuilds {
                if self.engine.rules().any(|r| r.name == name) {
                    // User op is already durable. A failed second commit must
                    // not surface as the caller's error.
                    if let Err(e) =
                        self.log_then_apply(WalRecord::RebuildRule { name: name.clone() })
                    {
                        eprintln!(
                            "auto-rebuild of rule {name:?} failed after durable user commit: {e}"
                        );
                        failed.push(name);
                    }
                }
            }
            for name in failed {
                self.engine.queue_rebuild_needed(name);
            }
        }
        Ok(())
    }

    /// Install a post-commit hook. Replaces any previous sink.
    ///
    /// The sink runs inside `log_then_apply` after a successful
    /// durable commit, while the caller still holds `&mut self`. When this
    /// database is behind a [`crate::SharedDb`], that means the **write
    /// guard is held**. The sink must never call `read` / `write` (or any
    /// other method) on the same `SharedDb` — the `RwLock` is not
    /// re-entrant and doing so deadlocks. The sink is `Send + Sync`;
    /// `std::sync::mpsc::Sender` is not `Sync` and will not type-check.
    /// Intended examples: `std::sync::mpsc::SyncSender`,
    /// `tokio::sync::mpsc::Sender`, `tokio::sync::broadcast::Sender`
    /// (non-blocking `send`), or `Arc<Mutex<Vec<MutationEvent>>>`.
    pub fn set_event_sink(&mut self, sink: Box<dyn Fn(MutationEvent) + Send + Sync>) {
        self.event_sink = Some(sink);
    }

    /// Whether a post-commit event sink is currently installed.
    pub fn has_event_sink(&self) -> bool {
        self.event_sink.is_some()
    }

    /// Set WAL fsync cadence. Default [`FsyncPolicy::Strict`].
    pub fn set_fsync_policy(&mut self, p: FsyncPolicy) {
        self.fsync = p;
    }

    /// Return the current WAL fsync cadence.
    pub fn fsync_policy(&self) -> FsyncPolicy {
        self.fsync
    }

    // ── Group-commit event deferral ───────────────────────────────────────────

    /// Enable or disable deferred event mode.
    ///
    /// When `true`, event notifications (subscription `DbEvent`s and legacy
    /// `MutationEvent` sink calls) are buffered rather than fired immediately.
    /// Call [`flush_deferred_events`] after the group fsync to deliver them,
    /// or [`discard_deferred_events`] if the fsync failed and the group must
    /// be treated as lost.
    pub fn set_deferred_events_mode(&mut self, defer: bool) {
        self.defer_events = defer;
    }

    /// Fire all buffered events accumulated since [`set_deferred_events_mode`]
    /// was set to true.  Clears the buffer.
    ///
    /// Called by the drain thread AFTER a successful group fsync, so
    /// subscribers observe only data that is durably on disk.
    pub fn flush_deferred_events(&mut self) {
        let events = std::mem::take(&mut self.deferred_events);
        for de in events {
            self.distribute_events(&de.rec, &de.engine_deltas, de.seq);
            self.emit_committed(&de.rec, de.ingest);
        }
    }

    /// Discard all buffered events without firing them.
    ///
    /// Called by the drain thread when a group fsync fails: the WAL has been
    /// truncated back to the pre-group offset, so the committed-but-unsynced
    /// ops must not be observable to subscribers.
    pub fn discard_deferred_events(&mut self) {
        self.deferred_events.clear();
    }

    // ── Degraded state ────────────────────────────────────────────────────────

    /// Mark this database as degraded.
    ///
    /// Called by the group-commit drain thread after a group fsync failure and
    /// WAL truncation: the in-memory state is now ahead of the on-disk WAL, so
    /// further mutations would deepen the divergence.  All subsequent calls to
    /// [`log_then_apply_with`] return `Err` until the database is reopened.
    pub fn set_degraded(&mut self) {
        self.degraded = true;
    }

    fn emit(&self, ev: MutationEvent) {
        if let Some(sink) = &self.event_sink {
            sink(ev);
        }
    }

    fn emit_committed(&self, rec: &WalRecord, ingest: Option<(String, usize)>) {
        match rec {
            WalRecord::Batch(inner) => {
                for r in inner {
                    if let Some(ev) = event_from_record(r, &self.syms, &self.ids) {
                        self.emit(ev);
                    }
                }
                match ingest {
                    Some((label, inserted)) => {
                        self.emit(MutationEvent::Ingested { label, inserted })
                    }
                    None => {
                        let ops = inner
                            .iter()
                            .filter(|r| !matches!(r, WalRecord::Intern { .. }))
                            .count();
                        if ops > 1 {
                            self.emit(MutationEvent::BatchApplied { ops });
                        }
                    }
                }
            }
            other => {
                if let Some(ev) = event_from_record(other, &self.syms, &self.ids) {
                    self.emit(ev);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Subscription API
    // -----------------------------------------------------------------------

    /// Distribute post-commit events to all live subscribers.
    ///
    /// Build a row-key → row-data map from a [`ResultSet`].
    ///
    /// Each row is serialized to JSON to form its key; a debug fallback is used
    /// if serialization fails. Used by both the initial-seed path in
    /// [`Self::subscribe_query`] and the per-commit diff path in
    /// [`Self::distribute_events`] to keep the two in sync.
    fn result_to_row_map(
        result: &core_query::ResultSet,
    ) -> std::collections::HashMap<String, Vec<Option<Value>>> {
        (0..result.len())
            .map(|i| {
                let row = result.row(i).to_vec();
                let key = serde_json::to_string(&row).unwrap_or_else(|_| format!("{row:?}"));
                (key, row)
            })
            .collect()
    }

    /// Collect the set of label syms touched by a WAL record.
    ///
    /// Returns `Some(set)` when every record in this commit can be attributed to
    /// a known label sym. Returns `None` when the commit must not be skipped:
    /// edge records, unresolvable key→label lookups, or any record type not in
    /// the explicit handled set.
    ///
    /// Handled record types and their actions:
    /// - `InsertNode`   → look up label in interner (fails → None)
    /// - `InsertNodeId` → label sym is carried directly
    /// - `SetProp`      → resolve key→id→label (fails → None)
    /// - `DeleteNode`   → resolve key→id→label (fails → None)
    /// - `Batch`        → recurse into every inner record
    /// - `InsertEdge`, `DeleteEdge`, `InsertEdgeId` → always None (edge records)
    /// - everything else → None (conservative)
    fn commit_touched_labels(
        rec: &WalRecord,
        syms: &Interner,
        ids: &IdMap,
        labels: &[u32],
    ) -> Option<BTreeSet<u32>> {
        let mut out = BTreeSet::new();
        if Self::collect_touched_labels(rec, syms, ids, labels, &mut out) {
            Some(out)
        } else {
            None
        }
    }

    fn collect_touched_labels(
        rec: &WalRecord,
        syms: &Interner,
        ids: &IdMap,
        labels: &[u32],
        out: &mut BTreeSet<u32>,
    ) -> bool {
        match rec {
            // String-key insert: the dense rewrite converts this to
            // [Intern, InsertNodeId], so this arm fires only for legacy WAL
            // records written before the dense path was added.
            WalRecord::InsertNode { label, .. } => {
                if let Some(sym) = syms.get(label) {
                    out.insert(sym);
                    true
                } else {
                    false
                }
            }
            // Dense-id insert (produced by rewrite_wal_dense for every
            // insert_node call in the current codebase).
            WalRecord::InsertNodeId { label, .. } => {
                out.insert(*label);
                true
            }
            // String-key prop set: dense path converts to [Intern, SetPropId].
            WalRecord::SetProp { key, .. } => {
                if let Some(sym) = Self::resolve_key_label_sym(key, ids, labels) {
                    out.insert(sym);
                    true
                } else {
                    false
                }
            }
            // Dense-id prop set (produced by rewrite_wal_dense for set_prop).
            WalRecord::SetPropId { id, .. } => {
                if let Some(sym) = labels.get(*id as usize).copied().filter(|&s| s != u32::MAX) {
                    out.insert(sym);
                    true
                } else {
                    false
                }
            }
            WalRecord::DeleteNode { key } => {
                if let Some(sym) = Self::resolve_key_label_sym(key, ids, labels) {
                    out.insert(sym);
                    true
                } else {
                    false
                }
            }
            WalRecord::Batch(inner) => inner
                .iter()
                .all(|r| Self::collect_touched_labels(r, syms, ids, labels, out)),
            // Intern is a pure metadata record — it does not touch any node's
            // label and is safe to skip for the label-skip predicate.
            WalRecord::Intern { .. } => true,
            // Edge records: always re-execute (edges can change join results).
            WalRecord::InsertEdge { .. }
            | WalRecord::DeleteEdge { .. }
            | WalRecord::InsertEdgeId { .. } => false,
            _ => false,
        }
    }

    /// Resolve a node key to its label sym via the dense id table.
    /// Returns `None` if the key is unknown or the label is a tombstone sentinel.
    fn resolve_key_label_sym(key: &str, ids: &IdMap, labels: &[u32]) -> Option<u32> {
        let id = ids.get(key)?;
        let sym = labels.get(id as usize).copied()?;
        (sym != u32::MAX).then_some(sym)
    }

    /// Distribute post-commit events to all live subscribers.
    ///
    /// Called from `log_then_apply_with` after apply + fsync, before the
    /// legacy MutationEvent sink. Prunes dead `Weak` entries in-place.
    ///
    /// Query subscriptions (subscribe_query) re-execute their plan on every
    /// call and diff the result against the previous run. Zero overhead when
    /// no query subscriptions are active.
    fn distribute_events(&mut self, rec: &WalRecord, engine_deltas: &[EngineEdgeDelta], seq: u64) {
        if self.subscriptions.is_empty() && self.query_subscriptions.is_empty() {
            return;
        }

        if !self.subscriptions.is_empty() {
            // Build write events from the WAL record.
            let write_events: Vec<DbEvent> =
                Self::write_events_from_record(rec, seq, &self.syms, &self.ids);

            // Build edge events from engine deltas.  Weight is looked up from
            // edge_props at distribution time (after apply), so it's always fresh.
            let edge_events: Vec<DbEvent> = engine_deltas
                .iter()
                .map(|d| {
                    if d.fired {
                        // The score lives under the rule's declared weight_prop,
                        // which is not always the literal "weight".
                        let prop = self
                            .engine
                            .rules()
                            .find(|r| r.name == d.rule)
                            .and_then(|r| r.weight_prop.as_deref());
                        let weight = prop.and_then(|p| {
                            self.edge_props
                                .get(d.etype_sym, d.src_id, d.dst_id, p)
                                .and_then(|v| {
                                    if let core_storage::Value::Float(f) = v {
                                        Some(*f)
                                    } else {
                                        None
                                    }
                                })
                        });
                        DbEvent::EdgeFired {
                            rule: d.rule.clone(),
                            src_key: d.src_key.clone(),
                            dst_key: d.dst_key.clone(),
                            edge_type: d.edge_type.clone(),
                            weight,
                            commit_seq: seq,
                        }
                    } else {
                        DbEvent::EdgeRetracted {
                            rule: d.rule.clone(),
                            src_key: d.src_key.clone(),
                            dst_key: d.dst_key.clone(),
                            edge_type: d.edge_type.clone(),
                            commit_seq: seq,
                        }
                    }
                })
                .collect();

            // Prune dead entries; push matching events to live ones.
            self.subscriptions.retain(|entry| {
                let Some(inner) = entry.inner.upgrade() else {
                    return false;
                };
                for ev in &write_events {
                    if event_matches(ev, &entry.filter) {
                        inner.push(ev.clone());
                    }
                }
                for ev in &edge_events {
                    if event_matches(ev, &entry.filter) {
                        inner.push(ev.clone());
                    }
                }
                true
            });

            // Turn off delta accumulation if all subscribers dropped and no views remain.
            if self.subscriptions.is_empty() && self.view_store.is_empty() {
                self.engine.set_emit_deltas(false);
            }
        }

        // Query subscriptions: full re-run per commit, then diff rows.
        // IMPORTANT: full re-execution on every commit — use LIMIT to bound cost.
        // Differential evaluation is roadmap / Phase 5.
        if !self.query_subscriptions.is_empty() {
            // Take the list out so we can call self.view() without borrow conflict.
            let mut query_subs = std::mem::take(&mut self.query_subscriptions);
            let empty_params = BTreeMap::new();
            query_subs.retain_mut(|entry| {
                let Some(inner) = entry.inner.upgrade() else {
                    return false; // subscriber dropped — prune
                };
                // Label-skip: if the plan has a known scan label and this commit
                // can be proven to touch only different labels (and no rule-derived
                // edge deltas fired), the result set cannot have changed — skip.
                if let Some(scan_sym) = entry.scan_label {
                    if engine_deltas.is_empty() {
                        let touched =
                            Self::commit_touched_labels(rec, &self.syms, &self.ids, &self.labels);
                        if touched.map(|t| !t.contains(&scan_sym)).unwrap_or(false) {
                            return true; // safe to skip — result set unchanged
                        }
                    }
                }
                QUERY_SUB_EXECS_TL.with(|c| c.set(c.get() + 1));
                let result = match execute(&self.view(), &entry.ops, &Params(&empty_params)) {
                    Ok(r) => r,
                    Err(e) => {
                        // Keep the subscription alive; skip the diff for this commit.
                        // Re-run errors are transient (e.g., planner change) and
                        // self-heal when the next commit succeeds.
                        eprintln!("[mushroomdb] subscribe_query re-run failed: {e}");
                        return true;
                    }
                };
                // Build new row map: serialized-key → row data.
                let new_row_map = Self::result_to_row_map(&result);
                // Removed rows: in prev but not in new.
                for (key, row) in &entry.prev_row_map {
                    if !new_row_map.contains_key(key) {
                        inner.push(DbEvent::QueryRowRemoved {
                            columns: entry.columns.clone(),
                            row: row.clone(),
                        });
                    }
                }
                // Added rows: in new but not in prev.
                for (key, row) in &new_row_map {
                    if !entry.prev_row_map.contains_key(key) {
                        inner.push(DbEvent::QueryRowAdded {
                            columns: entry.columns.clone(),
                            row: row.clone(),
                        });
                    }
                }
                entry.prev_row_map = new_row_map;
                true
            });
            self.query_subscriptions = query_subs;
        }
    }

    /// Returns `true` if any live subscriber or view definition requires delta
    /// accumulation. Used to set `engine.emit_deltas` on subscribe/view DDL.
    fn needs_emit_deltas(&self) -> bool {
        !self.view_store.is_empty()
            || self
                .subscriptions
                .iter()
                .any(|e| e.inner.upgrade().is_some())
    }

    /// Convert a WAL record into `DbEvent` write events with the given seq.
    fn write_events_from_record(
        rec: &WalRecord,
        seq: u64,
        intern: &Interner,
        ids: &IdMap,
    ) -> Vec<DbEvent> {
        match rec {
            WalRecord::InsertNode { label, key, .. } => vec![DbEvent::NodeInserted {
                label: label.clone(),
                key: key.clone(),
                commit_seq: seq,
            }],
            // *Id arms run after a successful apply, so resolution can only
            // fail on a programming error. Skip the event rather than emit a
            // fabricated "" that clients can't tell from a real empty value
            // (mirrors event_from_record returning None).
            WalRecord::InsertNodeId { label, key, .. } => intern
                .resolve(*label)
                .map(|label| DbEvent::NodeInserted {
                    label: label.to_string(),
                    key: key.clone(),
                    commit_seq: seq,
                })
                .into_iter()
                .collect(),
            WalRecord::SetProp { key, field, .. } => vec![DbEvent::PropSet {
                key: key.clone(),
                field: field.clone(),
                commit_seq: seq,
            }],
            WalRecord::SetPropId { id, field, .. } => ids
                .key_of(*id)
                .zip(intern.resolve(*field))
                .map(|(key, field)| DbEvent::PropSet {
                    key: key.to_string(),
                    field: field.to_string(),
                    commit_seq: seq,
                })
                .into_iter()
                .collect(),
            WalRecord::RemoveProp { key, field } => vec![DbEvent::PropRemoved {
                key: key.clone(),
                field: field.clone(),
                commit_seq: seq,
            }],
            WalRecord::InsertEdge {
                edge_type,
                src_key,
                dst_key,
            } => vec![DbEvent::EdgeInserted {
                edge_type: edge_type.clone(),
                src: src_key.clone(),
                dst: dst_key.clone(),
                commit_seq: seq,
            }],
            WalRecord::InsertEdgeId { etype, src, dst } => (|| {
                Some(DbEvent::EdgeInserted {
                    edge_type: intern.resolve(*etype)?.to_string(),
                    src: ids.key_of(*src)?.to_string(),
                    dst: ids.key_of(*dst)?.to_string(),
                    commit_seq: seq,
                })
            })()
            .into_iter()
            .collect(),
            WalRecord::DeleteEdge {
                edge_type,
                src_key,
                dst_key,
            } => vec![DbEvent::EdgeDeleted {
                edge_type: edge_type.clone(),
                src: src_key.clone(),
                dst: dst_key.clone(),
                commit_seq: seq,
            }],
            WalRecord::DeleteNode { key } => vec![DbEvent::NodeDeleted {
                key: key.clone(),
                commit_seq: seq,
            }],
            WalRecord::Batch(inner) => inner
                .iter()
                .flat_map(|r| Self::write_events_from_record(r, seq, intern, ids))
                .collect(),
            WalRecord::CreateRule { .. }
            | WalRecord::DeleteRule { .. }
            | WalRecord::RebuildRule { .. }
            | WalRecord::CreateView { .. }
            | WalRecord::DeleteView { .. }
            | WalRecord::EnableFulltext { .. }
            | WalRecord::DisableFulltext { .. }
            | WalRecord::EnableIndex { .. }
            | WalRecord::DisableIndex { .. }
            | WalRecord::CreateConstraint { .. }
            | WalRecord::DropConstraint { .. }
            | WalRecord::EnableRangeIndex { .. }
            | WalRecord::DisableRangeIndex { .. }
            | WalRecord::Intern { .. }
            // History markers produce no DbEvent — the engine delta already
            // fired the EdgeFired/EdgeRetracted subscription events.
            | WalRecord::DerivedEdgeAdded { .. }
            | WalRecord::DerivedEdgeRetracted { .. }
            // A count is not an edge event: the pair it counts already fired one
            // when it was first inserted.
            | WalRecord::SetEdgeCount { .. }
            | WalRecord::RenameNode { .. } => vec![],
        }
    }

    /// Subscribe to edge-fire and edge-retract events for one named rule.
    ///
    /// Returns `Err(GraphError::RuleNotFound)` if `rule_name` is not
    /// currently registered. Dropping the returned [`Subscription`] handle
    /// unregisters the subscriber — no further events are queued, no
    /// resources leak.
    pub fn subscribe_rule(&mut self, rule_name: &str) -> core_storage::Result<Subscription> {
        if self.read_only {
            return Err(core_storage::GraphError::ReadOnly);
        }
        if !self.engine.rules().any(|r| r.name == rule_name) {
            return Err(core_storage::GraphError::RuleNotFound {
                name: rule_name.to_string(),
            });
        }
        let inner = SubInner::new(self.sub_capacity());
        self.subscriptions.push(SubEntry {
            filter: SubFilter::Rule(rule_name.to_string()),
            inner: std::sync::Arc::downgrade(&inner),
        });
        self.engine.set_emit_deltas(true);
        Ok(Subscription(inner))
    }

    /// Subscribe to edge-fire and edge-retract events for **all** rules.
    ///
    /// Returns `Err(GraphError::ReadOnly)` if called on an as-of instance —
    /// as-of instances never commit, so `distribute_events` never runs and the
    /// subscription would never deliver events.
    pub fn subscribe_all_rules(&mut self) -> core_storage::Result<Subscription> {
        if self.read_only {
            return Err(core_storage::GraphError::ReadOnly);
        }
        let inner = SubInner::new(self.sub_capacity());
        self.subscriptions.push(SubEntry {
            filter: SubFilter::AllRules,
            inner: std::sync::Arc::downgrade(&inner),
        });
        self.engine.set_emit_deltas(true);
        Ok(Subscription(inner))
    }

    /// Subscribe to write events: node insert/delete, prop set/remove.
    ///
    /// Does not include edge-fire / edge-retract (rule-derived edge events).
    ///
    /// Returns `Err(GraphError::ReadOnly)` if called on an as-of instance —
    /// as-of instances never commit, so `distribute_events` never runs and the
    /// subscription would never deliver events.
    pub fn subscribe_writes(&mut self) -> core_storage::Result<Subscription> {
        if self.read_only {
            return Err(core_storage::GraphError::ReadOnly);
        }
        let inner = SubInner::new(self.sub_capacity());
        self.subscriptions.push(SubEntry {
            filter: SubFilter::Writes,
            inner: std::sync::Arc::downgrade(&inner),
        });
        self.engine.set_emit_deltas(true);
        Ok(Subscription(inner))
    }

    /// Subscribe to incremental Cypher query results.
    ///
    /// Parses and plans `cypher`; rejects the query if the plan is not in the
    /// allowlisted subset (see [`core_query::cypher::is_subscribable`]):
    ///   - `MATCH (n:Label) WHERE … RETURN … [LIMIT n]`
    ///   - `MATCH (a)-[r:TYPE]->(b) RETURN … [LIMIT n]`  (exactly one hop)
    ///
    /// SKIP is not supported — it shifts the result window on every commit,
    /// causing spurious Added/Removed churn for rows whose data never changed.
    /// Multi-hop Expand chains are not supported; each additional MATCH clause
    /// widens scope beyond the documented single-scan / single-hop subset.
    ///
    /// After each successful commit, the plan is **fully re-executed** and the
    /// result is diffed against the previous run. Added rows produce
    /// [`DbEvent::QueryRowAdded`]; removed rows produce
    /// [`DbEvent::QueryRowRemoved`].
    ///
    /// **Full re-run per commit; use LIMIT to bound execution cost.**
    /// The existing 1 M intermediate-row cap applies. Differential evaluation
    /// is roadmap / Phase 5.
    ///
    /// Returns `Err(GraphError::ReadOnly)` if called on an as-of instance —
    /// as-of instances never commit, so `distribute_events` never runs and the
    /// subscription would never deliver events.
    ///
    /// Returns `Err(GraphError::QueryError)` if the query fails to parse, plan,
    /// or if the plan shape is not in the allowlist.
    pub fn subscribe_query(&mut self, cypher: &str) -> Result<Subscription> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        let ast = parse(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        let ops = plan(&ast).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        if !is_subscribable(&ops) {
            return Err(GraphError::QueryError {
                detail: "subscribe_query only supports allowlisted plan shapes: \
                         MATCH (n:Label) WHERE … RETURN … [LIMIT n] or \
                         MATCH (a)-[r:TYPE]->(b) RETURN … [LIMIT n] (exactly one hop). \
                         Not supported: multi-hop Expand chains, SKIP (creates \
                         unstable offset windows), ORDER BY, DISTINCT, aggregates, \
                         variable-length paths, OPTIONAL MATCH, WITH, UNWIND. \
                         Use LIMIT to bound re-execution cost."
                    .to_string(),
            });
        }
        // Execute once to capture initial state (initial rows are not emitted as
        // events — the subscriber learns the baseline via the first query call).
        let empty_params = BTreeMap::new();
        let initial = execute(&self.view(), &ops, &Params(&empty_params)).map_err(|e| {
            GraphError::QueryError {
                detail: format!("execute: {e}"),
            }
        })?;
        let columns = initial.columns().to_vec();
        let prev_row_map = Self::result_to_row_map(&initial);
        let inner = SubInner::new(self.sub_capacity());
        // Derive the scan-label sym for the commit-skip fast-path.  Any Expand op
        // or unrecognized leading scan → None (always re-execute).
        let scan_label = extract_scan_label(&ops, Arc::make_mut(&mut self.syms));
        self.query_subscriptions.push(QuerySubEntry {
            ops,
            columns,
            prev_row_map,
            inner: std::sync::Arc::downgrade(&inner),
            scan_label,
        });
        Ok(Subscription(inner))
    }

    /// Queue capacity used for new subscriptions.
    fn sub_capacity(&self) -> usize {
        self.sub_capacity
    }

    /// Override per-subscriber queue capacity for subsequently created
    /// subscriptions on this db instance.
    ///
    /// Default is [`DEFAULT_SUB_CAPACITY`] (65,536 events). Use a smaller
    /// value in tests to exercise the [`DbEvent::Lagged`] path without
    /// generating tens of thousands of events.
    ///
    /// This is a test-support escape hatch. Calling it in production reduces
    /// subscriber reliability (more Lagged events). It is hidden from rustdoc
    /// to discourage accidental production use.
    #[doc(hidden)]
    pub fn set_sub_capacity(&mut self, capacity: usize) {
        self.sub_capacity = capacity;
    }

    // -----------------------------------------------------------------------

    /// Start an atomic batch.
    ///
    /// The returned [`BatchBuilder`] borrows `self` mutably until
    /// [`BatchBuilder::commit`]. Builder methods queue ops only — no
    /// validation, no WAL I/O. `commit` validates every queued op against
    /// live state plus preceding ops in this batch (duplicate key inside
    /// the batch is `Err`; an edge between two nodes created earlier in
    /// the batch is valid; `delete_node` then insert of the same key is a
    /// fresh identity). Validation never mutates the database. Any failure
    /// leaves WAL bytes and in-memory state identical to before `commit`.
    /// On success, one `WalRecord::Batch` frame is appended (one fsync)
    /// and each inner record is applied in order so rules fire per record.
    /// An empty batch, or a batch of only no-ops, writes zero WAL bytes.
    ///
    /// **Rule-window limitation:** batch validation cannot see edges that a
    /// rule created earlier in the *same* batch will derive at apply time, so
    /// a `delete_edge` / `insert_edge` in that window is silently no-oped
    /// where sequential calls would return `Err(RuleOwned)`. State integrity
    /// is unaffected (idempotent apply, provenance intact). Create rules in
    /// their own batch, or sequentially, when later ops may touch derived
    /// edges.
    pub fn batch(&mut self) -> BatchBuilder<'_, F> {
        BatchBuilder {
            db: self,
            ops: Vec::new(),
        }
    }

    /// Closure-style atomic write batch.
    ///
    /// Equivalent to calling [`GraphDb::batch`], invoking `build` to queue ops,
    /// then committing. All ops queued inside `build` are validated in order and
    /// committed as a single `WalRecord::Batch` frame (one fsync). Rules fire
    /// once per inner record, in order, after commit — semantically identical to
    /// sequential single-op writes.
    ///
    /// **Error semantics — validate-then-apply.** `build` queues ops without
    /// touching the database. [`BatchBuilder::commit`] validates every op against
    /// live state plus earlier ops in this batch before writing anything. If op N
    /// fails validation (duplicate key, unknown key, rule-owned edge, …) the
    /// entire batch is rejected: no WAL bytes are written and no in-memory state
    /// changes. The database is identical to its state before `write_batch` was
    /// called.
    ///
    /// **Atomicity is crash-level, NOT isolation-level.** On replay after a crash,
    /// a partial (torn) `Batch` frame applies NONE of its ops — the frame is
    /// either fully applied or not at all. However, while applying a committed
    /// batch, concurrent readers may observe intermediate states as ops are applied
    /// sequentially in memory. There is no interactive transaction isolation in v1.
    /// This is documented as "crash-atomic write batches; no interactive
    /// transactions or read isolation."
    ///
    /// **Returns** `(nodes_inserted, edges_inserted)`. An empty or all-noop batch
    /// writes zero WAL bytes and returns `(0, 0)`.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (nodes, edges) = db.write_batch(|b| {
    ///     b.insert_node("Person", "alice", vec![("age".into(), Value::Int(30))]);
    ///     b.insert_node("Person", "bob", vec![]);
    ///     b.insert_edge("KNOWS", "alice", "bob");
    ///     b.set_prop("alice", "role", Value::Str("admin".into()));
    ///     b.delete_node("old_key");
    /// })?;
    /// // One fsync; on crash replay: all five ops land or none do.
    /// ```
    pub fn write_batch<C>(&mut self, build: C) -> Result<(usize, usize)>
    where
        C: FnOnce(&mut BatchBuilder<'_, F>),
    {
        let mut b = self.batch();
        build(&mut b);
        b.commit()
    }

    /// Insert `rows` as nodes of `label`. One call is one atomic batch:
    /// auto-declared KeyMatch rules (if any) first, then the accepted node
    /// inserts, so incremental fire sees the new rules. Per-row key problems
    /// are collected in [`IngestReport::row_errors`] and skipped; a commit
    /// `Err` means nothing was applied.
    ///
    /// Auto-FK rule names are `auto_fk_<src_label_lowercase>_<field>` so
    /// distinct source labels sharing an FK field each get their own rule.
    pub fn ingest(
        &mut self,
        label: &str,
        rows: Vec<BTreeMap<String, Value>>,
        opts: &IngestOptions,
    ) -> Result<IngestReport> {
        self.ingest_with_edges(label, rows, opts, &[])
    }

    /// [`ingest`] plus user edges in the **same** previewed WAL batch.
    /// A failing edge rejects the whole request; nothing is applied.
    pub fn ingest_with_edges(
        &mut self,
        label: &str,
        rows: Vec<BTreeMap<String, Value>>,
        opts: &IngestOptions,
        edges: &[(String, String, String)],
    ) -> Result<IngestReport> {
        crate::ingest::run(self, label, rows, opts, edges)
    }

    /// Parse `json` as an array of objects and ingest via [`GraphDb::ingest`].
    ///
    /// JSON `null` fields are silently omitted (not stored, not a row error).
    /// Nested objects and arrays-of-objects are a per-row error (row skipped).
    /// Parse failures and a top-level value that is not an array of objects
    /// return [`GraphError::IngestError`].
    pub fn ingest_json(
        &mut self,
        label: &str,
        json: &str,
        opts: &IngestOptions,
    ) -> Result<IngestReport> {
        crate::ingest::run_json(self, label, json, opts)
    }

    fn commit_logged_batch(
        &mut self,
        ops: Vec<BatchOp>,
        ingest: Option<(String, usize)>,
        // Two-source rule: write_batch_authz threads authz here directly (never
        // touches pending_write_authz); query_write_authz sets the field instead
        // and passes None.  Only one source is non-None per call.
        param_authz: Option<WriteAuthz>,
    ) -> Result<BatchOutcome> {
        // Read-only guard: catches empty-batch calls before the early-return
        // that skips log_then_apply_with, ensuring all mutation entry points fail.
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // Ensure provenance is decoded before MutPreview accesses it
        // (note_delete_rule / is_rule_owned may call engine.provenance()).
        self.load_provenance_for_write();

        // ── Authz pre-check ──────────────────────────────────────────────────
        // Evaluate the decision table per-op BEFORE MutPreview so that a denial
        // produces no WAL frame (all-or-nothing at the authz boundary extends
        // the existing validate-then-apply contract to role-scope checks).
        //
        // `batch_created` tracks key→label for nodes created by earlier ops in
        // THIS batch, so InsertEdgeUpsert can count same-batch placeholder nodes
        // as visible without needing to call `self.ids.get` on not-yet-committed
        // keys (they won't be there yet).
        //
        // Two-source rule: param_authz (write_batch_authz path) takes precedence;
        // fall back to self.pending_write_authz (query_write_authz/Cypher path).
        // Cloning the field copy avoids a simultaneous borrow of self.ids below.
        let authz_opt = param_authz.or_else(|| self.pending_write_authz.clone());
        if let Some(ref authz) = authz_opt {
            let mut batch_created: BTreeMap<String, String> = BTreeMap::new();
            for op in &ops {
                self.check_single_op_authz(authz, op, &batch_created)?;
                // Update batch_created after a passing authz check so that
                // subsequent ops in this batch see the nodes as "about to exist".
                match op {
                    BatchOp::InsertNode { label, key, .. } => {
                        // Only track genuinely new nodes (absent from the
                        // snapshot at authz-check time). A pre-existing visible
                        // key would be a DuplicateKey — not a real creation —
                        // so MutPreview handles it. Letting it into batch_created
                        // would allow a later SetProp to bypass update_labels
                        // via the "batch-created → always updatable" ruling
                        // (delete+recreate exploit, fix for I1 review round 2).
                        //
                        // Accepted edge: for a delete+recreate-with-different-
                        // label batch, node_status resolves the pre-delete
                        // (store) label for any subsequent update checks. This
                        // grants no net-new capability — a role that can delete+
                        // create can already place arbitrary props via
                        // InsertNode's own props field.
                        if self.ids.get(key.as_str()).is_none() {
                            batch_created.insert(key.clone(), label.clone());
                        }
                    }
                    BatchOp::InsertEdgeUpsert {
                        placeholder_label,
                        src_key,
                        dst_key,
                        ..
                    } => {
                        // Both endpoints will be created if not already in store.
                        for ep_key in [src_key, dst_key] {
                            if self.ids.get(ep_key.as_str()).is_none()
                                && !batch_created.contains_key(ep_key.as_str())
                            {
                                batch_created.insert(ep_key.clone(), placeholder_label.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut outcome = BatchOutcome::default();
        let recs = {
            let mut preview = MutPreview::new(self);
            let mut recs = Vec::with_capacity(ops.len());
            // Which node row we are on, counted over the node-insert ops only.
            // A caller that queues its rows in order reads this straight back
            // as the index into its own list.
            let mut node_row = 0usize;
            // Every field name the store knows, which a `Replace` needs to work
            // out what it removes. Resolved on the first `Replace` in the frame
            // and reused, so N replaces read the field list once, not N times.
            let mut store_fields: Option<Vec<String>> = None;
            // Duplicate inserts this frame has to count, each paired with the
            // position in `recs` it belongs at. The count itself is named in the
            // dense rewrite and not here: a duplicate's endpoints and edge type
            // may all be created by earlier ops in this same frame, and nothing
            // in the frame has a dense id yet. See [`PlannedRec`].
            let mut deferred_counts: Vec<(usize, String, String, String)> = Vec::new();
            for op in ops {
                match op {
                    BatchOp::InsertNode { label, key, props } => {
                        node_row += 1;
                        preview.check_insert_node(&key, &props)?;
                        preview.note_insert_node(&label, &key, &props);
                        recs.push(WalRecord::InsertNode { label, key, props });
                    }
                    BatchOp::InsertNodeOnConflict {
                        label,
                        key,
                        props,
                        on_conflict,
                    } => {
                        let row = node_row;
                        node_row += 1;
                        if !preview.has_key(&key) {
                            // No conflict: an ordinary insert on any policy —
                            // except that a supplied view-owned field is the
                            // same mistake here as on a taken key, and gets the
                            // same row error rather than a frame error. Without
                            // this, one op answered one request two ways
                            // depending on whether the store already had the
                            // key (defect #19).
                            if let Some(why) = preview.supplied_view_owned_prop(&key, &props) {
                                outcome.row_errors.push((row, why));
                                continue;
                            }
                            preview.note_insert_node(&label, &key, &props);
                            recs.push(WalRecord::InsertNode { label, key, props });
                            continue;
                        }
                        match on_conflict {
                            OnConflict::Error => {
                                return Err(GraphError::DuplicateKey { key });
                            }
                            OnConflict::Skip => outcome.skipped += 1,
                            OnConflict::Replace => {
                                if store_fields.is_none() {
                                    store_fields = Some(preview.db.props_view().field_names());
                                }
                                let fields = store_fields.as_deref().unwrap_or_default();
                                match preview.plan_replace(&label, &key, &props, fields) {
                                    Ok((writes, kept_view_owned)) => {
                                        outcome.kept_view_owned += kept_view_owned;
                                        for (field, value) in writes {
                                            match value {
                                                Some(value) => {
                                                    preview.note_set_prop(&key, &field, &value);
                                                    recs.push(WalRecord::SetProp {
                                                        key: key.clone(),
                                                        field,
                                                        value,
                                                    });
                                                }
                                                None => {
                                                    preview.note_remove_prop(&key, &field);
                                                    recs.push(WalRecord::RemoveProp {
                                                        key: key.clone(),
                                                        field,
                                                    });
                                                }
                                            }
                                        }
                                        outcome.replaced += 1;
                                    }
                                    Err(why) => outcome.row_errors.push((row, why)),
                                }
                            }
                        }
                    }
                    BatchOp::InsertEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if preview.prepare_insert_edge(&edge_type, &src_key, &dst_key)? {
                            preview.note_insert_edge(&edge_type, &src_key, &dst_key);
                            recs.push(WalRecord::InsertEdge {
                                edge_type,
                                src_key,
                                dst_key,
                            });
                        } else if preview.db.multiplicity {
                            // A duplicate inside a batch counts the way a
                            // duplicate through `insert_edge` does: `ingest` and
                            // Cypher `CREATE` reach this choke-point and not
                            // that one, and a count only one entry point keeps
                            // would be worse than no count at all.
                            //
                            // This is the one gate on discriminant 23 from the
                            // batch path: a store that never opted in queues
                            // nothing here and so writes no such record.
                            deferred_counts.push((recs.len(), edge_type, src_key, dst_key));
                        }
                    }
                    BatchOp::SetProp { key, field, value } => {
                        if let Some(view_name) = preview.db.view_store.view_for_prop(&field) {
                            return Err(GraphError::ViewPropReadOnly {
                                view_name: view_name.to_string(),
                            });
                        }
                        preview.check_live_key(&key)?;
                        preview.note_set_prop(&key, &field, &value);
                        recs.push(WalRecord::SetProp { key, field, value });
                    }
                    BatchOp::RemoveProp { key, field } => {
                        if preview.prepare_remove_prop(&key, &field)? {
                            preview.note_remove_prop(&key, &field);
                            recs.push(WalRecord::RemoveProp { key, field });
                        }
                    }
                    BatchOp::DeleteEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if preview.prepare_delete_edge(&edge_type, &src_key, &dst_key)? {
                            preview.note_delete_edge(&edge_type, &src_key, &dst_key);
                            recs.push(WalRecord::DeleteEdge {
                                edge_type,
                                src_key,
                                dst_key,
                            });
                        }
                    }
                    BatchOp::DeleteNode { key } => {
                        preview.check_live_key(&key)?;
                        preview.note_delete_node(&key);
                        recs.push(WalRecord::DeleteNode { key });
                    }
                    BatchOp::CreateRule(def) => {
                        preview.check_create_rule(&def)?;
                        let def_bytes =
                            bincode::serialize(&def).map_err(|e| GraphError::Corrupt {
                                detail: format!("serialize rule: {e}"),
                            })?;
                        preview.note_create_rule(&def);
                        recs.push(WalRecord::CreateRule { def_bytes });
                    }
                    BatchOp::DeleteRule { name } => {
                        preview.check_delete_rule(&name)?;
                        preview.note_delete_rule(&name);
                        recs.push(WalRecord::DeleteRule { name });
                    }
                    BatchOp::RenameNode { old_key, new_key } => {
                        preview.check_rename_node(&old_key, &new_key)?;
                        // A stored `id` that equals the key is the key, written
                        // as a property by `upsert_entity`'s create path and by
                        // Cypher `CREATE {id: …}`. It moves with the key, in
                        // this frame; left behind, `WHERE n.id = '<new key>'`
                        // finds nothing (ledger row 72). An `id` holding
                        // anything else is the caller's and stays.
                        let id_follows = preview.prop_value(&old_key, "id")
                            == Some(Value::Str(old_key.clone()))
                            && preview.db.view_store.view_for_prop("id").is_none();
                        preview.note_rename_node(&old_key, &new_key);
                        recs.push(WalRecord::RenameNode {
                            old_key,
                            new_key: new_key.clone(),
                        });
                        if id_follows {
                            let value = Value::Str(new_key.clone());
                            preview.note_set_prop(&new_key, "id", &value);
                            recs.push(WalRecord::SetProp {
                                key: new_key,
                                field: "id".into(),
                                value,
                            });
                        }
                    }
                    BatchOp::InsertEdgeUpsert {
                        edge_type,
                        src_key,
                        dst_key,
                        placeholder_label,
                    } => {
                        // Auto-create any missing endpoints as plain InsertNode ops.
                        // Rules fire and last-change is updated for each created node.
                        for key in [&src_key, &dst_key] {
                            if !preview.has_key(key) {
                                // A placeholder endpoint carries no props, so
                                // the view-owned check has nothing to refuse.
                                preview.check_insert_node(key, &[])?;
                                preview.note_insert_node(&placeholder_label, key, &[]);
                                recs.push(WalRecord::InsertNode {
                                    label: placeholder_label.clone(),
                                    key: key.clone(),
                                    props: vec![],
                                });
                            }
                        }
                        if preview.prepare_insert_edge(&edge_type, &src_key, &dst_key)? {
                            preview.note_insert_edge(&edge_type, &src_key, &dst_key);
                            recs.push(WalRecord::InsertEdge {
                                edge_type,
                                src_key,
                                dst_key,
                            });
                        } else if preview.db.multiplicity {
                            // Same choke-point, same gate as `BatchOp::InsertEdge`
                            // above: an upsert that finds the pair already there
                            // is a duplicate insert and counts as one.
                            deferred_counts.push((recs.len(), edge_type, src_key, dst_key));
                        }
                    }
                }
            }
            // Splice the deferred counts back into the positions they were
            // raised at, so a count still sits exactly where the duplicate did
            // — before any later op in the frame that deletes the pair.
            let mut planned: Vec<PlannedRec> =
                Vec::with_capacity(recs.len() + deferred_counts.len());
            let mut deferred = deferred_counts.into_iter().peekable();
            for (i, rec) in recs.into_iter().enumerate() {
                while deferred.peek().is_some_and(|(at, ..)| *at == i) {
                    let (_, edge_type, src_key, dst_key) = deferred.next().expect("just peeked");
                    planned.push(PlannedRec::DuplicateCount {
                        edge_type,
                        src_key,
                        dst_key,
                    });
                }
                planned.push(PlannedRec::Rec(rec));
            }
            for (_, edge_type, src_key, dst_key) in deferred {
                planned.push(PlannedRec::DuplicateCount {
                    edge_type,
                    src_key,
                    dst_key,
                });
            }
            planned
        };
        // A frame that is nothing but skips or refused rows writes no WAL, but
        // it still has counts to report, so the early returns carry `outcome`
        // rather than zeros.
        if recs.is_empty() {
            return Ok(outcome);
        }
        // rewrite_wal_dense converts every InsertNode/InsertEdge into its
        // *Id form, so only the dense variants can appear in `recs` here.
        let recs = self.rewrite_wal_dense_planned(recs)?;
        // The rewrite can empty a non-empty batch: a `SET n.ns` naming the
        // namespace the node is already in is a no-op and is dropped there. An
        // empty `Batch` frame would still take a commit sequence and a WAL
        // record, so a batch that turns out to be nothing writes nothing.
        if recs.is_empty() {
            return Ok(outcome);
        }
        outcome.nodes_inserted = recs
            .iter()
            .filter(|r| matches!(r, WalRecord::InsertNodeId { .. }))
            .count();
        outcome.edges_inserted = recs
            .iter()
            .filter(|r| matches!(r, WalRecord::InsertEdgeId { .. }))
            .count();
        // Ingest / write_batch / query_write: one Batch frame, one fsync per call
        // under Strict.  Pass self.fsync directly so Strict stays Strict —
        // wal_needs_sync(Strict, _) always returns true regardless of op count.
        // Mapping Strict → Batched (the prior bug) caused wal_needs_sync to
        // short-circuit on single-op batches and silently skip the fsync.
        // Batched fsyncs only for multi-op batches; Relaxed always skips.
        self.log_then_apply_with(WalRecord::Batch(recs), ingest, self.fsync)?;
        Ok(outcome)
    }

    fn commit_batch(&mut self, ops: Vec<BatchOp>) -> Result<(usize, usize)> {
        self.commit_logged_batch(ops, None, None).map(inserted_pair)
    }

    /// Commit one submission WITHOUT an fsync — for use inside `commit_group`
    /// and the group-commit drain thread, which do a single group fsync later.
    fn commit_batch_nosync(&mut self, ops: Vec<BatchOp>) -> Result<(usize, usize)> {
        // Restore fsync policy even on panic via a raw-pointer drop guard.
        // A panic here would poison the RwLock anyway, but the correct policy
        // must be in place if the guard is ever unwrapped.
        struct RestoreFsync(*mut FsyncPolicy, FsyncPolicy);
        impl Drop for RestoreFsync {
            fn drop(&mut self) {
                // SAFETY: the pointer is valid for the full duration of
                // commit_batch_nosync; the guard is dropped before the frame
                // returns, and GraphDb outlives this frame.
                unsafe {
                    *self.0 = self.1;
                }
            }
        }
        let saved = self.fsync;
        // SAFETY: raw pointer into self; guard dropped within this frame.
        let _g = RestoreFsync(&mut self.fsync as *mut FsyncPolicy, saved);
        self.fsync = FsyncPolicy::Relaxed;
        self.in_nosync_group = true;
        let result = self.commit_logged_batch(ops, None, None).map(inserted_pair);
        self.in_nosync_group = false;
        result
    }

    /// Commit multiple op-batches as a **group**: each submission gets its own
    /// WAL `Batch` frame, but there is exactly **one** `Fs::sync` for the whole
    /// group (under `Strict` / `Batched` policy; `Relaxed` skips all syncs).
    ///
    /// # Durability semantics
    ///
    /// A crash before the group fsync may lose **all** submissions in the group.
    /// A crash after the group fsync preserves all of them.  No submission is
    /// ever torn: each WAL frame is either fully applied on replay or dropped
    /// in its entirety (CRC-protected frame boundaries).
    ///
    /// Events and subscription notifications fire per-submission immediately
    /// after apply, which may be before the group fsync.  From a subscriber's
    /// perspective this is equivalent to the `Relaxed` durability window.
    /// Submitters using [`SharedDb::submit_batch`] only unblock after the group
    /// fsync, so from their perspective durability is fully guaranteed.
    ///
    /// # MVCC interplay
    ///
    /// Each submission records its own `CommitDelta`; the fold-every-K counter
    /// increments per submission (not per group), preserving existing reader
    /// snapshot semantics.
    ///
    /// # Returns
    ///
    /// One `Result<(nodes_inserted, edges_inserted)>` per input group element,
    /// in order.  Failures are per-submission (validation errors); the group
    /// fsync error (if any) is returned as the second tuple element.
    pub fn commit_group(
        &mut self,
        groups: Vec<Vec<BatchOp>>,
    ) -> (Vec<Result<(usize, usize)>>, Option<GraphError>) {
        if let Err(e) = self.upgrade_to_v11_for_ops(groups.iter().map(Vec::as_slice)) {
            return (group_refusal(e, groups.len()), None);
        }
        let mut results = Vec::with_capacity(groups.len());
        for ops in groups {
            results.push(self.commit_batch_nosync(ops));
        }
        let any_ok = results.iter().any(|r| r.is_ok());
        let sync_err = if self.fsync != FsyncPolicy::Relaxed && any_ok {
            let r = self
                .fs
                .sync(core_storage::fs::FileId::Wal)
                .map_err(GraphError::Io)
                .err();
            if r.is_none() {
                self.note_wal_synced();
            }
            r
        } else {
            None
        };
        (results, sync_err)
    }

    /// Like [`commit_group`] but skips the group fsync entirely.
    ///
    /// Used by the drain thread to apply submissions under the write lock and
    /// then perform the single fsync OUTSIDE the lock (via
    /// `core_storage::sync_wal_at`), reducing the write-lock hold time visible
    /// to concurrent readers.
    pub fn commit_group_nosync(
        &mut self,
        groups: Vec<Vec<BatchOp>>,
    ) -> Vec<Result<(usize, usize)>> {
        // Ahead of the first append, as `commit_group` and the drain do: the
        // upgrade fsyncs the WAL before its snapshot, so the snapshot holds
        // only durable state, never a submission of this group.
        if let Err(e) = self.upgrade_to_v11_for_ops(groups.iter().map(Vec::as_slice)) {
            return group_refusal(e, groups.len());
        }
        let mut results = Vec::with_capacity(groups.len());
        for ops in groups {
            results.push(self.commit_batch_nosync(ops));
        }
        results
    }

    pub fn insert_node(
        &mut self,
        label: &str,
        key: &str,
        props: Vec<(String, Value)>,
    ) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        MutPreview::new(self).check_insert_node(key, &props)?;
        self.log_dense(vec![WalRecord::InsertNode {
            label: label.into(),
            key: key.into(),
            props,
        }])
    }

    /// Insert a user edge. `Ok(true)` when the pair was new, `Ok(false)` when it
    /// was already there — the question is "was this pair new", and a duplicate
    /// does not make it so.
    ///
    /// On a store that called [`enable_multiplicity`](Self::enable_multiplicity)
    /// a duplicate is no longer a total no-op: it raises the pair's insert count
    /// (§5.13). Adjacency is still a set, so [`degree`](Self::degree) is
    /// unchanged and the return value is still `Ok(false)`; the count is visible
    /// only through [`degree_multiplicity`](Self::degree_multiplicity) and the
    /// reserved [`EDGE_COUNT_PROP`]. On every other store a duplicate writes
    /// nothing at all, as it always has.
    pub fn insert_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        self.load_provenance_for_write();
        if !MutPreview::new(self).prepare_insert_edge(edge_type, src_key, dst_key)? {
            // The pair exists. The only thing left to record is that it was
            // asked for again, and only where the store asked to be told.
            if let Some(rec) = self.edge_count_record(edge_type, src_key, dst_key) {
                self.log_then_apply(rec)?;
            }
            return Ok(false);
        }
        self.log_dense(vec![WalRecord::InsertEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        }])?;
        Ok(true)
    }

    pub fn set_prop(&mut self, key: &str, field: &str, value: Value) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if let Some(view_name) = self.view_store.view_for_prop(field) {
            return Err(GraphError::ViewPropReadOnly {
                view_name: view_name.to_string(),
            });
        }
        MutPreview::new(self).check_live_key(key)?;
        self.log_dense(vec![WalRecord::SetProp {
            key: key.into(),
            field: field.into(),
            value,
        }])
    }

    /// Set several properties on one live node in a single WAL commit.
    ///
    /// Every per-property check [`set_prop`](Self::set_prop) runs — view-owned
    /// names, live key, the `ns` immutability rule and its type — is evaluated
    /// for the whole list before any record is logged. The first refusal
    /// returns and the node is unchanged. An empty list writes nothing.
    pub fn set_props(&mut self, key: &str, props: Vec<(String, Value)>) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        MutPreview::new(self).check_live_key(key)?;
        for (field, _) in &props {
            if let Some(view_name) = self.view_store.view_for_prop(field) {
                return Err(GraphError::ViewPropReadOnly {
                    view_name: view_name.to_string(),
                });
            }
        }
        if props.is_empty() {
            return Ok(());
        }
        self.write_batch(|b| {
            for (field, value) in props {
                b.set_prop(key, &field, value);
            }
        })
        .map(|_| ())
    }

    /// Remove a property. Returns `Ok(false)` (and does not log) if the field
    /// is already absent. Unknown or tombstoned keys are `Err(KeyNotFound)`.
    /// A field a view owns is `Err(ViewPropReadOnly)` — stated once, in
    /// [`MutPreview::prepare_remove_prop`], so that the batch ops reaching that
    /// same choke-point cannot miss it.
    pub fn remove_prop(&mut self, key: &str, field: &str) -> Result<bool> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !MutPreview::new(self).prepare_remove_prop(key, field)? {
            return Ok(false);
        }
        self.log_then_apply(WalRecord::RemoveProp {
            key: key.into(),
            field: field.into(),
        })?;
        Ok(true)
    }

    /// Delete a user edge. Returns `Ok(false)` (and does not log) if the edge
    /// is absent. Unknown keys are `Err(KeyNotFound)`. Rule-owned edges — in
    /// provenance, or a pair a live rule would derive — are `Err(RuleOwned)`
    /// (the rule would just put the edge back; delete or change the rule).
    pub fn delete_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        self.load_provenance_for_write();
        if !MutPreview::new(self).prepare_delete_edge(edge_type, src_key, dst_key)? {
            return Ok(false);
        }
        self.log_then_apply(WalRecord::DeleteEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        })?;
        Ok(true)
    }

    /// Delete a live node. Unknown or already-tombstoned keys are
    /// `Err(KeyNotFound)` and are not logged. Validation runs before the WAL
    /// write; `apply` of a logged `DeleteNode` for an already-tombstoned key
    /// (crash window) is a clean no-op.
    ///
    /// Returns a [`DeleteReport`] with counts of manual and derived edges
    /// removed (computed from live state before the deletion is applied).
    pub fn delete_node(&mut self, key: &str) -> Result<DeleteReport> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // Provenance must be loaded before we query provenance_touching.
        self.load_provenance_for_write();
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;

        // Count total graph edges touching this node before the delete so we can
        // derive the manual-edges count.  We also count direct derived edges (those
        // where `id` is src or dst in provenance) for the manual-edges subtraction;
        // via-hop derived edges (where `id` is the via node) do not appear in
        // provenance_touching, so they are excluded from this pre-delete scan.
        // The post-apply retraction count (ledger row 50) captures both.
        let tv = self.topo_view();
        let direct_derived_in_graph: u64 = self
            .engine
            .provenance_touching(id)
            .filter(|&(_, etype, src, dst)| tv.neighbors(etype, Direction::Out, src).contains(&dst))
            .count() as u64;

        let mut total_topo = 0u64;
        for et in tv.etypes() {
            total_topo += tv.neighbors(et, Direction::Out, id).len() as u64
                + tv.neighbors(et, Direction::In, id).len() as u64;
        }
        // For symmetric rules (e.g. Overlap), a→b and b→a are two separate directed
        // triples in both the topo scan (Out and In from id) and in provenance_touching.
        // The subtraction remains correct because both counts include both directions.
        //
        // Via-hop derived edges are not in `direct_derived_in_graph`, so they are
        // not subtracted here — they count as manual edges in the pre-delete view,
        // which would be wrong.  We tolerate this because the final `derived_edges`
        // value comes from the post-apply retraction count, which includes via-hop
        // edges, and `manual_edges` is clamped by saturating_sub.
        let manual_edges = total_topo.saturating_sub(direct_derived_in_graph);

        self.log_then_apply(WalRecord::DeleteNode { key: key.into() })?;
        // Use the engine's retraction count from the apply: includes direct and
        // via-hop derived edges removed by the delete (ledger row 50).
        let derived_edges = self.last_apply_retraction_count;
        Ok(DeleteReport {
            manual_edges,
            derived_edges,
        })
    }

    /// Rename a live node's key.  The dense id (and therefore all edges,
    /// props, history, and last-change tracking) is unaffected.
    ///
    /// A stored `id` property that equals `old` is rewritten to `new` in the
    /// same commit, so `n.id` and `key(n)` keep agreeing; an `id` holding any
    /// other value, and a node that stores none, are left as they are.
    ///
    /// Returns `Err(KeyNotFound)` if `old` is not a live key.
    /// Returns `Err(DuplicateKey)` if `new` is already live.
    pub fn rename_node(&mut self, old: &str, new: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // The `id` rewrite lives in the batch arm, which HTTP reaches without
        // this function. A node with no such `id` keeps the bare frame.
        if self.get_prop(old, "id") == Some(Value::Str(old.to_string())) {
            return self
                .commit_batch(vec![BatchOp::RenameNode {
                    old_key: old.into(),
                    new_key: new.into(),
                }])
                .map(|_| ());
        }
        MutPreview::new(self).check_rename_node(old, new)?;
        self.log_then_apply(WalRecord::RenameNode {
            old_key: old.into(),
            new_key: new.into(),
        })
    }

    /// Return the IVF drift counter for the dst-side candidate index of `rule`.
    /// `None` if the rule does not exist or is not approximate.
    ///
    /// The drift counter increments on IVF insert/remove after the last fit.
    /// When dst-side drift exceeds [`core_rules::IVF_DRIFT_REBUILD`], apply
    /// WAL-logs `RebuildRule` as a second commit (rebuild resets the counter).
    pub fn ivf_dst_drift(&self, rule: &str) -> Option<u64> {
        // SideIvfExport = (centroids, node→cluster, drift)
        self.engine
            .export_ivf_state()
            .remove(rule)
            .map(|(_src, dst)| dst.2)
    }

    /// Validate and WAL-log a new rule, then backfill derived edges inside apply.
    /// Validation and duplicate-name check run before logging so invalid rules
    /// never enter the WAL.
    pub fn create_rule(&mut self, def: RuleDef) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        MutPreview::new(self).check_create_rule(&def)?;
        let def_bytes = bincode::serialize(&def).map_err(|e| GraphError::Corrupt {
            detail: format!("serialize rule: {e}"),
        })?;
        self.log_dense(vec![WalRecord::CreateRule { def_bytes }])
    }

    /// Override this handle's HNSW build-slice size, or `None` to restore
    /// [`core_rules::HNSW_BUILD_BATCH`].
    ///
    /// Exposed for tests that need a small slice without a large corpus; not
    /// part of the stable surface.
    #[doc(hidden)]
    pub fn set_hnsw_build_batch(&mut self, batch: Option<usize>) {
        self.engine.set_hnsw_build_batch(batch);
    }

    /// Rules whose vector index is still being built, in name order.
    ///
    /// The same list [`GraphDb::stats`] reports per rule in `building`.
    /// After a clean open this includes a build a snapshot cut short, so
    /// `serve`'s ticker can pump it without a write.
    pub fn builds_in_progress(&self) -> Vec<BuildProgress> {
        self.engine.builds_in_progress()
    }

    /// Advance any vector index still building and backfill each rule that
    /// finishes. Returns what is still outstanding.
    ///
    /// A map lookup when nothing is pending, so it is cheap to call on a timer.
    /// One write lock and at most [`core_rules::HNSW_BUILD_BATCH`] vector
    /// inserts per pending rule per call, so a caller can drive a large build
    /// to completion without ever holding the lock for more than a slice.
    ///
    /// A rule that finishes here is backfilled through the same
    /// `WalRecord::RebuildRule` second commit that IVF drift already uses, so
    /// its derived edges are produced by [`GraphDb::rebuild_rule`]'s code path
    /// and appear all at once.
    ///
    /// Every ordinary write pumps one slice on its own (see the post-commit
    /// hook in `log_then_apply_with`), so this is for quiescent stores and for
    /// operators who want the build finished before traffic arrives.
    pub fn pump_index_build(&mut self) -> Result<Vec<BuildProgress>> {
        Ok(self.pump_index_build_reporting()?.1)
    }

    /// [`GraphDb::pump_index_build`], also reporting the builds that **this**
    /// call finished, so a progress display can say so.
    ///
    /// A build can be registered and completed inside a single call — that is
    /// what a mid-build snapshot looks like on reopen, where the index scan
    /// finishes the graph and only the backfill is outstanding — and the
    /// outstanding list alone cannot show that anything happened.
    pub fn pump_index_build_reporting(
        &mut self,
    ) -> Result<(Vec<BuildProgress>, Vec<BuildProgress>)> {
        // A read-only handle cannot issue the `RebuildRule` a finished build
        // needs, so it would advance the index and then silently fail to
        // produce the edges. Refusing is the honest answer.
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        let finished = self.pump_one_slice();
        for done in &finished {
            // The index is whole but the rule still owns no edges. A failed
            // second commit must leave the rule re-pumpable rather than
            // silently edge-less, so the error is surfaced here — unlike the
            // post-commit hook, this call is not riding someone else's commit.
            self.log_then_apply(WalRecord::RebuildRule {
                name: done.rule.clone(),
            })?;
        }
        Ok((finished, self.engine.builds_in_progress()))
    }

    /// Run the deferred candidate-index build, if it is still owed, against the
    /// graph as it stands *now* — before the caller applies anything.
    ///
    /// A no-op bool test once the indexes are populated, which is after the
    /// first write of the handle's life, and for a store with no rules at all.
    fn populate_indexes_before_write(&mut self) {
        if !self.engine.needs_index_population() {
            return;
        }
        // The retained snapshot blobs arrive with the V8 base sections; without
        // them the scan would rebuild every graph the snapshot already holds.
        self.ensure_v8_base_sections_loaded();
        if !self.engine.needs_index_population() {
            return;
        }
        let mut eng = std::mem::take(&mut self.engine);
        {
            let gm = make_graph_mut(
                &self.ids,
                &mut self.syms,
                &self.labels,
                build_props_view(&self.props, &self.base),
                &mut self.topo,
                &self.base,
                &mut self.edge_props,
            );
            eng.populate_indexes(&gm);
        }
        self.engine = eng;
    }

    /// One slice of build work for every pending rule. Returns the rules whose
    /// index just became whole, which the caller must `RebuildRule`.
    ///
    /// Goes through the engine even with nothing pending when the indexes have
    /// not been populated yet: that call adopts the persisted graphs and, for
    /// an incomplete blob already registered at open, leaves the remainder to
    /// this slice rather than inserting it inline.
    fn pump_one_slice(&mut self) -> Vec<BuildProgress> {
        // The retained snapshot blobs — and the id count an interrupted build
        // is recognised against — arrive with the V8 base sections, which a
        // clean open reads lazily. Without this a freshly opened handle pumps
        // against empty retained state and concludes there is nothing to do,
        // which is precisely the store `build-index` exists for.
        self.ensure_v8_base_sections_loaded();
        let mut eng = std::mem::take(&mut self.engine);
        let finished = {
            let mut gm = make_graph_mut(
                &self.ids,
                &mut self.syms,
                &self.labels,
                build_props_view(&self.props, &self.base),
                &mut self.topo,
                &self.base,
                &mut self.edge_props,
            );
            eng.pump_index_build(&mut gm)
        };
        self.engine = eng;
        finished
    }

    /// Register a sliced build a snapshot cut short, from blobs with
    /// `complete == false`.
    ///
    /// Peeks the V8 mmap for incomplete entries without copying complete
    /// graphs. V5–V7 already hold the blobs in the engine from restore.
    fn register_outstanding_index_builds(&mut self) {
        if self.engine.indexes_populated() {
            return;
        }
        let extra = self.collect_incomplete_hnsw_blobs();
        let mut eng = std::mem::take(&mut self.engine);
        {
            let gm = make_graph_mut(
                &self.ids,
                &mut self.syms,
                &self.labels,
                build_props_view(&self.props, &self.base),
                &mut self.topo,
                &self.base,
                &mut self.edge_props,
            );
            eng.register_incomplete_hnsw_builds(&extra, &gm);
        }
        self.engine = eng;
    }

    /// Incomplete `(src, dst)` HNSW blobs from the V8 mmap, copied only when
    /// `complete` is false. Empty when there is no mmap base (V5–V7 uses the
    /// engine's retained map instead).
    fn collect_incomplete_hnsw_blobs(&self) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
        let Some(base) = &self.base else {
            return BTreeMap::new();
        };
        let Ok(archived) = base.hnsw_section() else {
            return BTreeMap::new();
        };
        archived
            .rules
            .iter()
            .filter_map(|e| {
                let src = e.src_blob.as_slice();
                let dst = e.dst_blob.as_slice();
                if core_rules::hnsw::hnsw_blob_complete(src) == Some(false)
                    || core_rules::hnsw::hnsw_blob_complete(dst) == Some(false)
                {
                    Some((e.name.as_str().to_string(), (src.to_vec(), dst.to_vec())))
                } else {
                    None
                }
            })
            .collect()
    }

    /// WAL-log rule deletion. Returns RuleNotFound if the rule does not exist.
    pub fn delete_rule(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        MutPreview::new(self).check_delete_rule(name)?;
        self.log_then_apply(WalRecord::DeleteRule { name: name.into() })
    }

    /// Return a snapshot of all registered rules.
    pub fn rules(&self) -> Vec<RuleDef> {
        self.engine.rules().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Rule suggestion API
    // -----------------------------------------------------------------------

    /// Profile the database and suggest linking rules with previewed edge counts.
    ///
    /// Uses the default seed ([`core_rules::SUGGEST_DEFAULT_SEED`]) for deterministic
    /// sampling. Suggestions are sorted by estimated edge count (descending).
    /// **NO auto-accept** — call [`GraphDb::create_rule`] explicitly to apply.
    pub fn suggest_rules(&self) -> Vec<core_rules::RuleSuggestion> {
        self.suggest_rules_seeded(core_rules::SUGGEST_DEFAULT_SEED)
    }

    /// Like [`suggest_rules`] but with a caller-supplied RNG seed for
    /// reproducibility. Same seed + same data = identical output.
    pub fn suggest_rules_seeded(&self, seed: u64) -> Vec<core_rules::RuleSuggestion> {
        self.suggest_rules_with_config(&core_rules::suggest::SuggestConfig::default(), seed)
            .suggestions
    }

    /// [`suggest_rules_seeded`] with a fully custom [`SuggestConfig`].
    ///
    /// Returns a [`core_rules::SuggestReport`] that includes both the candidate list
    /// and a `truncated` flag indicating whether the global budget fired before all
    /// candidates were evaluated.
    pub fn suggest_rules_with_config(
        &self,
        config: &core_rules::suggest::SuggestConfig,
        seed: u64,
    ) -> core_rules::SuggestReport {
        use std::collections::BTreeMap;

        // Collect (node_id, key) pairs per label, skipping tombstoned nodes.
        let mut label_nodes: BTreeMap<String, Vec<(u32, String)>> = BTreeMap::new();
        for id in 0..self.ids.len() as u32 {
            let Some(key) = self.ids.key_of(id) else {
                continue;
            };
            let Some(&sym) = self.labels.get(id as usize) else {
                continue;
            };
            if sym == u32::MAX {
                continue; // tombstoned
            }
            let Some(label) = self.syms.resolve(sym) else {
                continue;
            };
            label_nodes
                .entry(label.to_string())
                .or_default()
                .push((id, key.to_string()));
        }

        let existing = self.rules();
        let pv = build_props_view(&self.props, &self.base);
        let all_fields: Vec<String> = pv.field_names();

        core_rules::suggest::suggest_rules(
            &label_nodes,
            &|id, field| pv.get(id, field).map(|vr| vr.into_value()),
            &all_fields,
            &existing,
            config,
            seed,
        )
    }

    /// Recompute a rule's derived edges from scratch. WAL-logged so un-trip
    /// plus later mutations replay identically (rebuild is a pure function
    /// of state).
    ///
    /// Only exit from the tripped latch: if the full desired set fits the
    /// budget, it is applied completely and `tripped` clears; if it still
    /// exceeds the budget, provenance is left untouched and `tripped` stays
    /// true. Counts as a fire evaluation (see [`RuleStats::fires`]).
    /// Unknown rule → `RuleNotFound`, nothing logged.
    pub fn rebuild_rule(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !self.engine.rules().any(|r| r.name == name) {
            return Err(GraphError::RuleNotFound { name: name.into() });
        }
        self.log_then_apply(WalRecord::RebuildRule { name: name.into() })
    }

    // -----------------------------------------------------------------------
    // Materialized view API
    // -----------------------------------------------------------------------

    /// Register a new materialized property view, backfill its values for all
    /// existing nodes, and WAL-log the definition.
    ///
    /// # Errors
    /// - `ReadOnly`: called on an as-of instance.
    /// - `RuleInvalid`: name collision, view_prop collision, or invalid def.
    /// - `QueryError`: a unique constraint, an equality index or a range index
    ///   covers `(label, view_prop)`.
    pub fn create_view(&mut self, def: ViewDef) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // Pre-validate before WAL write.
        def.validate()
            .map_err(|e| GraphError::RuleInvalid { detail: e })?;
        if self.view_store.has_view(&def.name) {
            return Err(GraphError::RuleInvalid {
                detail: format!("view {:?} already exists", def.name),
            });
        }
        // A view writes its property inside `apply`, after the unique-constraint
        // check has run, so a constraint over it could not hold.
        if let Some(c) = self
            .constraints
            .iter()
            .find(|c| c.label == def.label && c.field == def.view_prop)
        {
            return Err(GraphError::QueryError {
                detail: format!(
                    "property '{}' on label '{}' is declared unique by constraint '{}'; a view \
                     cannot own it",
                    def.view_prop, def.label, c.name
                ),
            });
        }
        // A view writes its property without passing the property index, so a
        // range index over it would answer from stale postings until the next
        // open rebuilt them.
        if let Some(index) = self.prop_index.range_index_on(&def.label, &def.view_prop) {
            return Err(GraphError::QueryError {
                detail: format!(
                    "property '{}' on label '{}' is covered by range index '{index}'; a view \
                     cannot own it",
                    def.view_prop, def.label
                ),
            });
        }
        // The same for an equality index (ledger row 123).
        if self
            .prop_index
            .is_equality_declared(&def.label, &def.view_prop)
        {
            return Err(GraphError::QueryError {
                detail: format!(
                    "property '{}' on label '{}' is covered by an equality index; a view cannot \
                     own it",
                    def.view_prop, def.label
                ),
            });
        }
        if let Some(existing) = self.view_store.view_for_prop(&def.view_prop) {
            return Err(GraphError::RuleInvalid {
                detail: format!(
                    "view_prop {:?} is already used by view {:?}",
                    def.view_prop, existing
                ),
            });
        }
        // A view cannot take the name of a property nodes already carry —
        // in the snapshot or written since. Refused here, before the WAL
        // write: `apply` refuses some of it too, but by then the record is
        // logged, and the next open replays it and fails.
        //
        // "Carry" is a node that reads a value under the name. The column
        // alone does not count: a snapshot keeps the column of a view that
        // has since been deleted, with every value in it hidden, and that
        // name is free.
        let taken = {
            let pv = build_props_view(&self.props, &self.base);
            pv.field_names().contains(&def.view_prop)
                && (0..self.ids.len() as u32)
                    .any(|id| !self.ids.is_tombstoned(id) && pv.get(id, &def.view_prop).is_some())
        };
        if taken {
            return Err(GraphError::RuleInvalid {
                detail: format!(
                    "view_prop {:?} conflicts with an existing node property",
                    def.view_prop
                ),
            });
        }
        let def_bytes = bincode::serialize(&def).map_err(|e| GraphError::Corrupt {
            detail: format!("serialize view: {e}"),
        })?;
        // Enable delta accumulation before the view is registered so subsequent
        // incremental edge events reach view maintenance from this point onward.
        // (The backfill inside create_view reads topo directly; it does not rely
        // on pending deltas.)
        self.engine.set_emit_deltas(true);
        self.log_then_apply(WalRecord::CreateView { def_bytes })
    }

    /// Remove a named view and delete its values from every node.
    ///
    /// # Errors
    /// - `ReadOnly`: called on an as-of instance.
    /// - `RuleNotFound`: view does not exist.
    pub fn delete_view(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !self.view_store.has_view(name) {
            return Err(GraphError::RuleNotFound { name: name.into() });
        }
        let result = self.log_then_apply(WalRecord::DeleteView { name: name.into() });
        // After deletion, disable accumulation if no listeners remain.
        if !self.needs_emit_deltas() {
            self.engine.set_emit_deltas(false);
        }
        result
    }

    /// Snapshot of all registered view definitions.
    pub fn views(&self) -> Vec<ViewDef> {
        self.view_store.views().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Full-text-lite API
    // -----------------------------------------------------------------------

    /// Enable full-text indexing for all nodes of `label` on property `field`.
    ///
    /// After this call, every subsequent write to `(label, field)` is reflected
    /// in the index incrementally.  Existing nodes are backfilled immediately.
    /// The declaration is persisted as a WAL record. The index is carried by
    /// the next snapshot and read from it at open; until then it is rebuilt
    /// at open from the stored values.
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::RuleInvalid`]: `(label, field)` is already indexed.
    pub fn enable_fulltext(&mut self, label: &str, field: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if self.fulltext.is_enabled(label, field) {
            return Err(GraphError::RuleInvalid {
                detail: format!("full-text index for ({label:?}, {field:?}) already enabled"),
            });
        }
        self.log_then_apply(WalRecord::EnableFulltext {
            label: label.into(),
            field: field.into(),
        })
    }

    /// Disable full-text indexing for `(label, field)` and drop its postings.
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::RuleNotFound`]: `(label, field)` is not currently indexed.
    pub fn disable_fulltext(&mut self, label: &str, field: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !self.fulltext.is_enabled(label, field) {
            return Err(GraphError::RuleNotFound {
                name: format!("fulltext({label},{field})"),
            });
        }
        self.log_then_apply(WalRecord::DisableFulltext {
            label: label.into(),
            field: field.into(),
        })
    }

    /// Whether this handle read its full-text index from the snapshot it
    /// opened, and kept it, where it would otherwise have built it from the
    /// stored values. False for a store with no snapshot or no declared pair,
    /// for a snapshot written before the index was carried, and for an open
    /// that had to fall back. The answers are the same either way; this is
    /// how a test or an operator tells which open they got.
    pub fn fulltext_loaded_from_snapshot(&self) -> bool {
        self.fulltext_loaded
    }

    /// Called by the insert arms of `apply` before they index `field` on
    /// `id`. `add_tokens` appends, so a node already indexed under the field
    /// has to be cleared first or its old words stay searchable.
    ///
    /// Live, that is one insert naming a property twice: the last value is
    /// what is stored, and it is what is indexed. At open it means the WAL
    /// holds a write the loaded index already has; the index is rebuilt after
    /// the replay, and clearing one node at a time here would only add a walk
    /// of the vocabulary for every such insert, so it is counted and left.
    fn unindex_before_reindexing(&mut self, id: u32, field: &str) {
        if !self.fulltext.has_doc(id, field) {
            return;
        }
        if self.replaying_at_open {
            self.fulltext_reindexed_a_node = true;
            self.fulltext_reindexed_at_open += 1;
        } else {
            Arc::make_mut(&mut self.fulltext).remove_node_field(id, field);
        }
    }

    /// How many times the replay at open indexed a node that was already
    /// indexed under that field. Non-zero means the WAL held writes the
    /// loaded index already had, and the open rebuilt it. Exposed for tests;
    /// not part of the stable surface.
    #[doc(hidden)]
    pub fn fulltext_reindexed_during_open(&self) -> u64 {
        self.fulltext_reindexed_at_open
    }

    /// Read the full-text index out of the snapshot this handle just mapped,
    /// if it carries one that reads back and was built over this id space.
    /// Anything else leaves the index empty for the rebuild that follows.
    fn load_fulltext_from_base(&mut self) {
        let Some(base) = self.base.as_ref() else {
            return;
        };
        // A snapshot that kept its WAL and carries no mark of how much of it
        // it holds is replayed over in full. Every insert in that WAL is
        // already in the blob, so reading it would end in a rebuild anyway —
        // after making each replayed insert tokenize first. With a mark, the
        // frames the snapshot holds are not applied again and the index is as
        // good as any other snapshot's.
        if !self.base_wal_truncated && base.wal_mark().is_none() {
            return;
        }
        let bytes = match base.fulltext_bytes() {
            Ok(bytes) if !bytes.is_empty() => bytes,
            _ => return,
        };
        match core_storage::fulltext::FulltextIndex::from_blob(bytes) {
            Some((index, node_count)) if node_count as usize == self.ids.len() => {
                self.fulltext = Arc::new(index);
                self.schema_stale = true;
                self.fulltext_loaded = true;
            }
            _ => eprintln!(
                "mushroomdb: the snapshot's full-text index could not be read; rebuilding it"
            ),
        }
    }

    /// The full-text index as the bytes a snapshot carries, or nothing when
    /// no pair is declared.
    fn fulltext_blob(&self) -> Vec<u8> {
        if self.fulltext.enabled_pairs().next().is_none() {
            return Vec::new();
        }
        self.fulltext.to_blob(self.ids.len() as u32)
    }

    /// Remove every edge the snapshot holds to or from a node that has been
    /// deleted.
    ///
    /// There should be none. Through 0.7.1 a node deleted after a snapshot
    /// kept the edges that snapshot held (ledger row 95), and the next
    /// snapshot wrote them into its own base, where they answered a
    /// neighbour read with `topology id N has no key`. This is what repairs
    /// such a store. It asks for the neighbours of every retired id under
    /// every edge type, which on a store with half a million deleted nodes is
    /// over a second, so it runs only over a snapshot that does not say its
    /// writer sweeps them (`MappedBase::edges_swept`). The removal is in
    /// memory until the next snapshot, which no longer carries them and says
    /// so.
    fn drop_edges_of_retired_nodes(&mut self) {
        let mut doomed = Vec::new();
        {
            let tv = self.topo_view();
            let etypes: Vec<u32> = tv.etypes().collect();
            for n in self.ids.tombstoned_ids() {
                for et in &etypes {
                    for &dst in tv.neighbors(*et, Direction::Out, n).as_ref() {
                        doomed.push((*et, n, dst));
                    }
                    for &src in tv.neighbors(*et, Direction::In, n).as_ref() {
                        if src != n {
                            doomed.push((*et, src, n));
                        }
                    }
                }
            }
        }
        for (et, s, d) in doomed {
            Arc::make_mut(&mut self.topo).remove_edge(et, s, d);
            Arc::make_mut(&mut self.edge_props).remove_edge(et, s, d);
            self.view_store.on_edge_changed(
                et,
                s,
                d,
                false,
                Arc::make_mut(&mut self.props),
                &build_topo_view(&self.topo, &self.base),
                &self.ids,
                &self.syms,
                &self.labels,
                base_columns(&self.base),
            );
        }
    }

    /// What `snapshot.intent` says about the snapshot and the WAL on disk.
    ///
    /// The first value is the WAL that the snapshot already holds, when the
    /// intent names this snapshot and `wal` begins with exactly the WAL it
    /// names: a snapshot that was to replace its WAL and did not get that
    /// far. The second is `true` when the file holds anything else — an
    /// intent left over, which must not outlive this handle's next append.
    fn interrupted_snapshot(
        &self,
        wal: &[u8],
    ) -> Result<(Option<core_storage::v8::WalMark>, bool)> {
        let (standing, left_over) = self.standing_intent(wal)?;
        Ok((standing.map(|i| i.wal), left_over))
    }

    /// The intent in `snapshot.intent` that is true of the snapshot and the
    /// WAL on disk, if one is, and whether the file holds anything that is
    /// not.
    ///
    /// A file that cannot be read is an error, not an absent intent: taking
    /// it for absent would replay the WAL over the very snapshot the intent
    /// is there to protect.
    fn standing_intent(
        &self,
        wal: &[u8],
    ) -> Result<(Option<core_storage::v8::SnapshotIntent>, bool)> {
        let bytes = self.fs.read(FileId::SnapshotIntent)?;
        if bytes.is_empty() {
            return Ok((None, false));
        }
        // The WAL is in hand, so it is asked first. Only a record whose mark
        // it matches is worth reading the whole snapshot for — which leaves a
        // left-over intent, the usual kind, costing nothing at each open.
        let candidates: Vec<_> = core_storage::v8::SnapshotIntent::decode_all(&bytes)
            .into_iter()
            .filter(|i| {
                i.wal.matches(wal)
                    // The mark was taken at the end of a whole WAL.
                    && decode_all(&wal[..i.wal.len as usize]).1 as u64 == i.wal.len
            })
            .collect();
        let standing = if candidates.is_empty() {
            None
        } else {
            let snapshot = self.fs.read(FileId::Snapshot)?;
            candidates.into_iter().find(|i| i.names(&snapshot))
        };
        // Anything in the file beyond the one standing intent is left over:
        // a record of a snapshot that finished or never landed.
        let left_over = bytes.len() != if standing.is_some() { 32 } else { 0 };
        Ok((standing, left_over))
    }

    /// Empty `snapshot.intent` unless it is still true. Called with the write
    /// lock held, before an append.
    fn settle_snapshot_intent(&mut self) -> Result<()> {
        let wal = self.fs.read(FileId::Wal)?;
        let (standing, left_over) = self.standing_intent(&wal)?;
        if left_over {
            // Keep the one that is still true, drop the rest.
            let keep = standing.map(|i| i.encode()).unwrap_or_default();
            self.fs.write_atomic(FileId::SnapshotIntent, &keep)?;
        }
        self.intent_unsettled = false;
        Ok(())
    }

    /// Make the next snapshot on this handle fail at `at`, leaving the files
    /// as a crash there would. For tests; not part of the stable surface.
    #[doc(hidden)]
    pub fn stop_next_snapshot(&mut self, at: SnapshotStop) {
        self.snapshot_stop = Some(at);
    }

    /// Whether `(label, field)` is currently indexed for full-text search.
    pub fn is_fulltext_enabled(&self, label: &str, field: &str) -> bool {
        self.fulltext.is_enabled(label, field)
    }

    /// Every `(label, field)` pair with a live full-text index, sorted.
    ///
    /// Note that [`GraphDb::search`] is keyed by field alone — a pair only
    /// declares which nodes are *indexed*, so callers that want to search
    /// everything indexed should query each distinct field once.
    pub fn fulltext_pairs(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self.fulltext.enabled_pairs().cloned().collect();
        v.sort();
        v
    }

    /// Enable an equality index for all nodes of `label` on scalar property
    /// `field`. Subsequent `WHERE n.field = value` lookups become O(matches)
    /// instead of an O(N_label) scan. Existing nodes are backfilled; the
    /// declaration persists via WAL and the postings rebuild on re-open.
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::RuleInvalid`]: `(label, field)` is already indexed, by
    ///   an equality index or by a range index (named in the error).
    /// - [`GraphError::QueryError`]: a view owns `(label, field)`.
    pub fn enable_index(&mut self, label: &str, field: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // A view writes its property inside `apply` without passing the
        // property index, so an equality index over it would answer from
        // stale postings until the next open rebuilt them (ledger row 123).
        if let Some(view) = self
            .view_store
            .views()
            .find(|v| v.label == label && v.view_prop == field)
        {
            return Err(GraphError::QueryError {
                detail: format!(
                    "property '{field}' on label '{label}' is owned by view '{}'; an equality \
                     index cannot cover it",
                    view.name
                ),
            });
        }
        if self.prop_index.is_enabled(label, field) {
            // A range index already answers equality on the pair.
            if let Some(index) = self.prop_index.range_index_on(label, field) {
                return Err(GraphError::RuleInvalid {
                    detail: format!(
                        "property index for ({label:?}, {field:?}) already enabled: range index \
                         '{index}' covers it"
                    ),
                });
            }
            return Err(GraphError::RuleInvalid {
                detail: format!("property index for ({label:?}, {field:?}) already enabled"),
            });
        }
        self.log_then_apply(WalRecord::EnableIndex {
            label: label.into(),
            field: field.into(),
        })
    }

    /// Disable the equality index for `(label, field)` and drop its postings.
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::RuleNotFound`]: `(label, field)` is not currently indexed.
    pub fn disable_index(&mut self, label: &str, field: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !self.prop_index.is_equality_declared(label, field) {
            // A pair only a range index covers is withdrawn by that index's name.
            if let Some(index) = self.prop_index.range_index_on(label, field) {
                return Err(GraphError::QueryError {
                    detail: format!(
                        "the index on ({label}, {field}) is range index '{index}'; drop it with \
                         disable_range_index"
                    ),
                });
            }
            return Err(GraphError::RuleNotFound {
                name: format!("index({label},{field})"),
            });
        }
        // The index is what a unique constraint is checked against: dropping
        // it under a constraint would let duplicates in silently.
        if let Some(c) = self
            .constraints
            .iter()
            .find(|c| c.label == label && c.field == field)
        {
            return Err(GraphError::QueryError {
                detail: format!(
                    "the index on ({label}, {field}) backs unique constraint '{}'; drop the \
                     constraint first",
                    c.name
                ),
            });
        }
        self.log_then_apply(WalRecord::DisableIndex {
            label: label.into(),
            field: field.into(),
        })
    }

    /// Declare the range index `name` on `(label, field)`.
    ///
    /// A range index answers range (`<`, `<=`, `>`, `>=`), `STARTS WITH` and
    /// exact-match lookups over the field's scalar values, for nodes of
    /// `label`. Existing nodes are indexed at once; the declaration is one WAL
    /// record, and the postings are rebuilt from the stored values at every
    /// open. An equality index on the same pair shares the postings and is
    /// unaffected by this or by [`disable_range_index`](Self::disable_range_index).
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::QueryError`]: a range index named `name` already
    ///   exists; `name` begins with `eq_` or `ft_` (reserved for the names
    ///   `SHOW INDEXES` lists equality and full-text indexes under);
    ///   `(label, field)` already has a range index; or a view owns
    ///   `(label, field)`.
    ///
    /// # Format
    /// An `EnableRangeIndex` record is format-V11: the first declaration on a
    /// store that is not V11 yet writes a V11 snapshot first, and from then on
    /// the store cannot be read by 0.9.x (see [`is_v11`](Self::is_v11)).
    pub fn enable_range_index(&mut self, name: &str, label: &str, field: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if let Some(err) = ddl::reserved_index_name(name) {
            return Err(err);
        }
        if self.prop_index.range_index(name).is_some() {
            return Err(GraphError::QueryError {
                detail: format!("a range index named '{name}' already exists"),
            });
        }
        if let Some(existing) = self.prop_index.range_index_on(label, field) {
            return Err(GraphError::QueryError {
                detail: format!(
                    "property '{field}' on label '{label}' already has range index '{existing}'"
                ),
            });
        }
        // A view writes its property inside `apply` without passing the
        // property index, so the postings would go stale under it.
        if let Some(view) = self
            .view_store
            .views()
            .find(|v| v.label == label && v.view_prop == field)
        {
            return Err(GraphError::QueryError {
                detail: format!(
                    "property '{field}' on label '{label}' is owned by view '{}'; a range index \
                     cannot cover it",
                    view.name
                ),
            });
        }
        self.log_dense(vec![WalRecord::EnableRangeIndex {
            name: name.into(),
            label: label.into(),
            field: field.into(),
        }])
    }

    /// Withdraw the range index `name`. An equality index on the same pair —
    /// from [`enable_index`](Self::enable_index) or a unique constraint —
    /// stays; otherwise the pair is no longer indexed.
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::QueryError`]: no range index is named `name`.
    pub fn disable_range_index(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if self.prop_index.range_index(name).is_none() {
            return Err(GraphError::QueryError {
                detail: format!("no range index named '{name}'"),
            });
        }
        self.log_dense(vec![WalRecord::DisableRangeIndex { name: name.into() }])
    }

    /// The declared range indexes as `(name, label, field)`, sorted by name.
    pub fn range_indexes(&self) -> Vec<(String, String, String)> {
        self.prop_index
            .range_indexes()
            .map(|(name, (label, field))| (name.clone(), label.clone(), field.clone()))
            .collect()
    }

    /// The node keys the range index on `(label, field)` holds between `lo`
    /// and `hi`, in index order — the answer
    /// [`UnifiedIndex::lookup_range`](core_storage::UnifiedIndex::lookup_range)
    /// gives, by key. Lets tests compare the postings across a reopen, a
    /// snapshot and `open_at` until the planner reads them (0.10 TY-10).
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn range_lookup_keys_for_test(
        &self,
        label: &str,
        field: &str,
        lo: Option<(&Value, bool)>,
        hi: Option<(&Value, bool)>,
    ) -> Vec<String> {
        self.prop_index
            .lookup_range(label, field, lo, hi)
            .filter_map(|id| self.ids.key_of(id).map(str::to_string))
            .collect()
    }

    /// Declare a unique constraint: no two live nodes of `label` may hold equal
    /// values for `field`.
    ///
    /// The equality index on `(label, field)` that the check runs against is
    /// created with it when absent — by the replay of the one
    /// `CreateConstraint` record this logs, so the declaration and its index
    /// are one commit. From then on every write that would give a second node
    /// of `label` a value already held fails with
    /// [`GraphError::ConstraintViolation`] and writes nothing. Values compare
    /// as a query's `=` does: `Int` 2 and `Float` 2.0 are one value, so are the two zeros
    /// and every `Int` that rounds to a `Float`, `NaN` equals nothing, two `DateTime`s at one
    /// UTC instant are equal whatever their offsets, and a list, map or `Duration` never
    /// conflicts.
    ///
    /// The existing data is checked first: every live node of `label` is read,
    /// and if two hold equal values for `field` nothing is logged and the error
    /// lists the first 100 nodes involved, in node-id order, and counts them all.
    /// Declaring blocks writers for a time proportional to the label's node
    /// count (the scan, then the index build); readers are unaffected.
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::ConstraintCreationConflict`]: live nodes of `label` already
    ///   hold equal values for `field`; `violations` is each `(node_key, value)`.
    /// - [`GraphError::QueryError`]: a constraint named `name` already exists;
    ///   or a view owns `(label, field)`.
    ///
    /// # Format
    /// A `CreateConstraint` record is format-V11: the first declaration on a
    /// store that is not V11 yet writes a V11 snapshot first, and from then on
    /// the store cannot be read by 0.9.x (see [`is_v11`](Self::is_v11)).
    pub fn create_constraint(&mut self, name: &str, label: &str, field: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if self.constraints.iter().any(|c| c.name == name) {
            return Err(GraphError::QueryError {
                detail: format!("a constraint named '{name}' already exists"),
            });
        }
        // A view writes its property inside `apply`, after the check has run,
        // so the constraint could not hold over it.
        if let Some(view) = self
            .view_store
            .views()
            .find(|v| v.label == label && v.view_prop == field)
        {
            return Err(GraphError::QueryError {
                detail: format!(
                    "property '{field}' on label '{label}' is owned by view '{}'; a unique \
                     constraint cannot cover it",
                    view.name
                ),
            });
        }
        let (violations, total) = self.existing_unique_violations(label, field);
        if total > 0 {
            return Err(GraphError::ConstraintCreationConflict { violations, total });
        }
        self.log_dense(vec![WalRecord::CreateConstraint {
            name: name.into(),
            label: label.into(),
            field: field.into(),
        }])
    }

    /// The first [`CONSTRAINT_CONFLICTS_LISTED`] `(node_key, value)` of live
    /// nodes of `label` whose `field` equals another live node's, in node-id
    /// order, and the count of all of them.
    ///
    /// One pass over the label column and one props read per node of `label`;
    /// nodes are grouped by [`equality_bucket`] and compared with
    /// `values_equal` inside a bucket, the equality the write path judges
    /// by, so `Int` 2 beside `Float` 2.0 is reported. A bucket holds only
    /// values that round to one `f64` or share an index key, so the
    /// distinct values per bucket are few and the comparison does not go
    /// quadratic on a heavily duplicated value.
    fn existing_unique_violations(
        &self,
        label: &str,
        field: &str,
    ) -> (Vec<(String, Value)>, usize) {
        let Some(label_sym) = self.syms.get(label) else {
            return (Vec::new(), 0);
        };
        let props = self.props_view();
        // Per bucket: the distinct values seen (each with the ids holding a
        // value equal to it), kept as classes so a duplicate is one lookup
        // against a handful of representatives, not against every holder.
        // `forms` is every distinct stored form of the class's value (an `Int`
        // and the `Float` it equals), for the cross-class check below.
        struct Class {
            forms: Vec<Value>,
            holders: Vec<u32>,
        }
        let mut buckets: HashMap<EqualityBucket, Vec<Class>> = HashMap::new();
        for (id, &sym) in self.labels.iter().enumerate() {
            if sym != label_sym {
                continue;
            }
            let id = id as u32;
            if self.ids.key_of(id).is_none() {
                continue;
            }
            let Some(value) = props.get(id, field).map(|vr| vr.into_value()) else {
                continue;
            };
            let Some(bucket) = equality_bucket(&value) else {
                continue;
            };
            let classes = buckets.entry(bucket).or_default();
            match classes
                .iter_mut()
                .find(|class| values_equal(&class.forms[0], &value))
            {
                Some(class) => {
                    class.holders.push(id);
                    if !class.forms.contains(&value) {
                        class.forms.push(value);
                    }
                }
                None => classes.push(Class {
                    forms: vec![value],
                    holders: vec![id],
                }),
            }
        }
        let mut conflicting: Vec<u32> = Vec::new();
        for classes in buckets.values() {
            for (i, class) in classes.iter().enumerate() {
                // Equality past 2^53 is not transitive: `Int` 2^53 and
                // 2^53 + 1 are unequal, yet both equal `Float` 2^53. A class
                // whose any form equals any form of another conflicts with it.
                let crosses = classes.iter().enumerate().any(|(j, other)| {
                    j != i
                        && class
                            .forms
                            .iter()
                            .any(|a| other.forms.iter().any(|b| values_equal(a, b)))
                });
                if class.holders.len() > 1 || crosses {
                    conflicting.extend(&class.holders);
                }
            }
        }
        conflicting.sort_unstable();
        let total = conflicting.len();
        // Only the returned list is capped; the count above is of them all.
        let listed = conflicting
            .into_iter()
            .take(CONSTRAINT_CONFLICTS_LISTED)
            .filter_map(|id| {
                let key = self.ids.key_of(id)?.to_string();
                let value = props.get(id, field)?.into_value();
                Some((key, value))
            })
            .collect();
        (listed, total)
    }

    /// Withdraw the unique constraint `name`. Its backing equality index stays
    /// in place; [`disable_index`](Self::disable_index) drops it once no
    /// constraint uses it.
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::QueryError`]: no constraint is named `name`.
    pub fn drop_constraint(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !self.constraints.iter().any(|c| c.name == name) {
            return Err(GraphError::QueryError {
                detail: format!("no constraint named '{name}'"),
            });
        }
        self.log_dense(vec![WalRecord::DropConstraint { name: name.into() }])
    }

    /// The declared unique constraints, in declaration order.
    pub fn constraints(&self) -> &[UniqueConstraint] {
        &self.constraints
    }

    /// Whether this store has opted in to the V11 snapshot format — it has
    /// written a value of one of the five 0.10 types, a unique constraint or
    /// a range index, at any point in its history.
    ///
    /// `false` on every store that has not, which keeps writing the snapshot
    /// version 0.9.x reads (V9, or V10 after
    /// [`enable_multiplicity`](Self::enable_multiplicity)). Once `true` it
    /// stays `true`: see `docs/format-stability.md`.
    pub fn is_v11(&self) -> bool {
        self.v11_features
    }

    /// Whether `(label, field)` currently has an equality index.
    pub fn is_index_enabled(&self, label: &str, field: &str) -> bool {
        self.prop_index.is_enabled(label, field)
    }

    /// Every `(label, field)` pair with a live equality index, sorted.
    ///
    /// The enumerator [`is_index_enabled`](Self::is_index_enabled) never had:
    /// the MCP `schema` tool lists a store's indexes rather than probing them
    /// one guess at a time.
    pub fn index_pairs(&self) -> Vec<(String, String)> {
        self.prop_index.enabled_pairs().cloned().collect()
    }

    /// The dense id of a live key, `None` for an unknown or deleted one.
    ///
    /// Ids are allocated in insertion order and never reused, so the lowest
    /// live id among a set of keys is the oldest node — which is how identity
    /// resolution picks a canonical node without a timestamp field
    /// (`memory::identity`). Crate-private: an id is an engine detail, not
    /// something a caller should hold.
    pub(crate) fn dense_id(&self, key: &str) -> Option<u32> {
        self.ids.get(key)
    }

    /// Start recording insert-count multiplicity on this store (§5.13).
    ///
    /// Adjacency stays a set and nothing about an existing read changes: a
    /// duplicate [`insert_edge`](Self::insert_edge) still returns `Ok(false)`
    /// and still leaves [`degree`](Self::degree) alone. What it gains is that
    /// the duplicate is *counted*, as the reserved edge property
    /// [`EDGE_COUNT_PROP`], readable through
    /// [`degree_multiplicity`](Self::degree_multiplicity).
    ///
    /// # This is a one-way step, and that is why it is a call
    ///
    /// The count is durable, so it is written to the WAL — as discriminant 23,
    /// which no release before v0.6.10 knows. A reader meeting an unknown WAL
    /// discriminant cannot know what the record would have changed, so it
    /// cannot degrade the way an unreadable index blob can. **After this call
    /// the store can no longer be read by an older binary, and there is no call
    /// that undoes it.** Gating the record behind this method is what keeps
    /// that step a decision an operator makes when they want the feature,
    /// rather than one everybody takes by upgrading.
    ///
    /// # It fails loudly, and that costs a snapshot
    ///
    /// An older binary does not refuse discriminant 23 — it truncates the WAL
    /// at it and, with `repair_wal`, persists the truncation. So this call also
    /// writes a **V10 snapshot**, a version no earlier release knows, and it
    /// writes it *first*: the snapshot is read before the WAL, so an older
    /// binary stops at `snapshot: unsupported version 10` with the WAL
    /// untouched. Taking the snapshot before appending the record is what makes
    /// the guard unconditional — the store is never, at any interruption point,
    /// carrying the record without the stamp that announces it.
    ///
    /// The snapshot keeps the WAL (`keep_wal: true`): opting in is not a
    /// compaction, and history reachable by [`open_at`](Self::open_at) stays
    /// reachable. On a large store the call therefore costs one full snapshot
    /// write.
    ///
    /// # What it costs a store that archives
    ///
    /// Writing `snapshot.bin` is also how the archive path decides whether the
    /// store may have a *genesis chain* — whether `open_at` can replay
    /// archive-resident commits from empty state. The rule is conservative: a
    /// snapshot that was already on disk might have been a truncating one, and
    /// once the handle that took it is gone this binary cannot tell. A
    /// `keep_wal` snapshot taken by **this** handle is the case where it can, so
    /// opting in and then archiving **in the same session** keeps the chain.
    ///
    /// Opting in, closing the store, and archiving in a *later* session does
    /// not — but that is the answer any store with a prior snapshot gets, not
    /// something this call causes. A store that wants the chain should take its
    /// first archive in the session that opted in.
    ///
    /// Calling it on a store that has already opted in writes nothing and
    /// returns `Ok(())`: an operator should not have to ask first.
    ///
    /// # This call is not atomic, and an `Err` does not undo it
    ///
    /// There is no rollback here, and there never was one. An `Err` means this
    /// handle stopped believing the store is opted in — `self.multiplicity` is
    /// reset, so this handle reports `false` from then on — and nothing more. It
    /// says nothing about what reached disk. Two reachable failures leave the
    /// opt-in standing:
    ///
    /// * **The declaration landed and only its fsync failed.** `log_then_apply`
    ///   appends, then syncs; a failed barrier leaves `MULTIPLICITY_ENABLED`
    ///   already in `wal.bin`. The next open replays it and the store is opted
    ///   in. No archive is involved — this one predates the recovery below.
    /// * **The declaration never landed, but the V10 snapshot did, on a store
    ///   that already had an archive.** The open-time recovery in
    ///   `load_from_disk` reads V10-beside-an-archive as an interrupted archive
    ///   sequence and opts the store in.
    ///
    /// So a failed call may leave the opt-in on disk immediately (the first
    /// case) or conjure it at the next open (the second), and nothing puts the
    /// store back out. Treat `Err` as "the outcome is unknown", not as "nothing
    /// happened".
    ///
    /// **This is safe, and the ordering is the reason.** The V10 stamp is
    /// written *before* the declaration, so every one of these intermediate
    /// states is one an older binary refuses by name rather than truncates at.
    /// The failure direction costs a refusal, never a commit. That ordering is
    /// the property worth protecting, not the atomicity this call never had.
    ///
    /// **To know where the store stands, ask the store.** Reopen it and call
    /// [`is_multiplicity_enabled`](Self::is_multiplicity_enabled); that is the
    /// only answer that accounts for what reached disk.
    ///
    /// The one case that really does leave the store opted out is a failure with
    /// no archive present and no record written: a stray V10 snapshot remains,
    /// costing an older reader a refusal it did not strictly need, and *that*
    /// store's next snapshot rewrites at V9.
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - Anything [`snapshot_with`](Self::snapshot_with) can return, and
    ///   anything the WAL append or its fsync can return. See the atomicity
    ///   section above for what the store is left holding.
    pub fn enable_multiplicity(&mut self) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if self.multiplicity {
            return Ok(());
        }
        // The snapshot goes first, and the order is the guard.
        //
        // An older reader refuses a V10 snapshot by name and stops; it does not
        // refuse discriminant 23, it truncates the WAL at it. So the store must
        // never hold the record without the snapshot that announces it — not
        // even for the width of one fsync. Writing the snapshot before the
        // record makes the only reachable intermediate state "V10 snapshot, no
        // record", which is merely conservative: this binary reads it as a
        // store that has not opted in, and an older one refuses it.
        //
        // `keep_wal: true` because opting in is not a compaction: an operator
        // asking for multiplicity has not asked to lose the history `open_at`
        // can reach.
        self.multiplicity = true;
        let forced = self
            .snapshot_with(SnapshotOptions {
                keep_wal: true,
                ..SnapshotOptions::default()
            })
            .and_then(|()| self.log_then_apply(core_storage::wal::MULTIPLICITY_ENABLED));
        if forced.is_err() {
            // This handle stops believing it is opted in. That is all this line
            // does — it is not a rollback, and cannot be one: the declaration
            // may already be in `wal.bin` (the append succeeded and only the
            // fsync failed), and even when it is not, the V10 snapshot beside an
            // existing archive is enough for the open-time recovery to opt the
            // store in. See the "not atomic" section on this method.
            //
            // It fails in the safe direction either way: the V10 stamp reached
            // disk before anything a v0.6.9 reader would truncate at, so the
            // worst an interruption costs that reader is a refusal by name.
            self.multiplicity = false;
        }
        forced
    }

    /// Move this store to the V11 snapshot format, ahead of the first V11-only
    /// record it appends (contract C9).
    ///
    /// The same move [`enable_multiplicity`](Self::enable_multiplicity) makes
    /// for V10, for the same reason: a 0.9.x reader does not refuse a WAL
    /// frame it cannot decode, it truncates the WAL there. So the V11 snapshot
    /// goes first, keeping the WAL (`keep_wal: true` — moving to V11 is not a
    /// compaction, and `open_at` keeps its history), and the record follows.
    /// At no interruption point does the store hold the record without the
    /// stamp. On a large store the first V11 write therefore costs one full
    /// snapshot write.
    ///
    /// An `Err` appends nothing V11. If the V11 snapshot did not reach disk,
    /// this handle goes on believing the store is not V11; if it did, the store
    /// is V11 from then on (C9) and later snapshots stay V11, which costs an
    /// older reader a refusal and nothing else.
    fn upgrade_to_v11(&mut self) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // What the snapshot holds must be durable first: under a batched
        // fsync policy the WAL may hold acknowledged frames not yet synced,
        // and the snapshot is durable the moment it is written.
        match self.fs.sync(FileId::Wal) {
            Ok(()) => self.note_wal_synced(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(GraphError::Io(e)),
        }
        self.v11_features = true;
        let snapshot = self.snapshot_with(SnapshotOptions {
            keep_wal: true,
            ..SnapshotOptions::default()
        });
        if snapshot.is_err() {
            // Once a V11 stamp is on disk the store stays V11 (C9), even
            // though nothing V11 was written: a later snapshot from this handle
            // must not write V9 over it.
            let on_disk = self
                .fs
                .read_prefix(FileId::Snapshot, 6)
                .ok()
                .filter(|h| h.len() >= 6 && &h[0..4] == b"GDB1")
                .map(|h| u16::from_le_bytes([h[4], h[5]]));
            self.v11_features = on_disk == Some(core_storage::snapshot::VERSION_11);
        }
        snapshot
    }

    /// Move the store to V11 ahead of a group commit whose submissions carry a
    /// V11-only value, so the upgrade snapshot holds only state that is
    /// already durable — never a submission of the group itself.
    ///
    /// Called by [`commit_group`](Self::commit_group) and by the
    /// [`SharedDb`](crate::SharedDb) drain before the group's first append.
    /// `BatchOp` cannot declare a constraint or range index, so a value is
    /// the only V11-only record a group can carry.
    pub(crate) fn upgrade_to_v11_for_ops<'a>(
        &mut self,
        groups: impl IntoIterator<Item = &'a [BatchOp]>,
    ) -> Result<()> {
        if self.v11_features || self.read_only {
            return Ok(());
        }
        let carries = |op: &BatchOp| match op {
            BatchOp::InsertNode { props, .. } | BatchOp::InsertNodeOnConflict { props, .. } => {
                props
                    .iter()
                    .any(|(_, v)| core_storage::v11_only_type(v).is_some())
            }
            BatchOp::SetProp { value, .. } => core_storage::v11_only_type(value).is_some(),
            _ => false,
        };
        if groups.into_iter().any(|ops| ops.iter().any(carries)) {
            self.upgrade_to_v11()?;
        }
        Ok(())
    }

    /// What a V11 snapshot's section 18 carries for this store.
    fn snapshot_declarations(&self) -> core_storage::v8::SnapshotDeclarations {
        core_storage::v8::SnapshotDeclarations {
            multiplicity: self.multiplicity,
            constraints: self.constraints.clone(),
            indexes: self.prop_index.equality_pairs().cloned().collect(),
            range_indexes: self
                .prop_index
                .range_indexes()
                .map(|(name, (label, field))| (name.clone(), label.clone(), field.clone()))
                .collect(),
        }
    }

    /// Whether this store records insert-count multiplicity.
    ///
    /// `false` on every store that has not called
    /// [`enable_multiplicity`](Self::enable_multiplicity) — which is every
    /// store that did not ask for it, including one upgraded from an earlier
    /// release.
    pub fn is_multiplicity_enabled(&self) -> bool {
        self.multiplicity
    }

    /// How many times `(etype, src, dst)` has been inserted: the reserved
    /// `count` edge property, or 1 when it is absent.
    ///
    /// Answers 1 for a pair on a store that never opted in, which is the truth
    /// available there — the pair was inserted at least once, and the store
    /// kept no record of any second insert.
    fn edge_insert_count(&self, etype: u32, src: u32, dst: u32) -> u64 {
        match self.edge_props_view().get(etype, src, dst, EDGE_COUNT_PROP) {
            Some(Value::Int(n)) if n > 0 => n as u64,
            _ => 1,
        }
    }

    /// The `SetEdgeCount` record a duplicate insert of `(edge_type, src_key,
    /// dst_key)` should log, or `None` when nothing should be written.
    ///
    /// `None` when the store has not opted in, so **no discriminant-23 record
    /// is written at all** — the gate the whole feature rests on.
    ///
    /// The other two `None`s are unreachable from the one caller. This is the
    /// single-mutation path, where `prepare_insert_edge` has already refused a
    /// missing endpoint and an existing pair's edge type is necessarily
    /// interned. A batch is the case where a pair's endpoints and type can all
    /// be created by the same frame, and it does not come through here: it
    /// queues a [`PlannedRec::DuplicateCount`] and names the count in the dense
    /// rewrite, which is the only pass that knows the frame's own ids.
    fn edge_count_record(
        &self,
        edge_type: &str,
        src_key: &str,
        dst_key: &str,
    ) -> Option<WalRecord> {
        if !self.multiplicity {
            return None;
        }
        let etype = self.syms.get(edge_type)?;
        let src = self.ids.get(src_key)?;
        let dst = self.ids.get(dst_key)?;
        Some(WalRecord::SetEdgeCount {
            etype,
            src,
            dst,
            count: self.edge_insert_count(etype, src, dst).saturating_add(1),
        })
    }

    /// Search a full-text-indexed field.
    ///
    /// Returns `(node_key, match_count)` pairs sorted by match_count descending,
    /// ties broken by key (lexicographic).  Tombstoned nodes are excluded.
    ///
    /// **Query syntax:**
    /// - Space-separated terms are AND'd: `"foo bar"` requires both.
    /// - `OR` between terms forms disjunction: `"foo OR bar"` matches either.
    /// - Trailing `*` on a term is a prefix match: `"rust*"` matches `rustlang`, `rusty`.
    /// - `AND` keyword is accepted explicitly and is the default.
    /// - Tokenization is unicode-alphanumeric (same as index time); case-insensitive.
    ///
    /// **Unindexed field:** returns `Ok(vec![])` if `field` is not indexed.
    /// Pin: this is the documented, tested, stable behavior for v1.
    ///
    /// **Memory / performance:** O(postings) lookup; no scan.  The index is
    /// in-memory and proportional to total indexed text across all enabled fields.
    ///
    /// **v2 grammar:** supports `"phrase"`, `-negation`, `prefix*`, `OR`, `AND`.
    /// Results are BM25-scored (k1=1.2, b=0.75) and sorted by score descending,
    /// key ascending for deterministic tiebreaking.
    pub fn search(&self, field: &str, query: &str) -> Vec<(String, f64)> {
        // Resolve node_ids to keys (excluding tombstones) then re-sort by
        // (score DESC, key ASC) to give a deterministic, key-lexicographic
        // tiebreak.  FulltextIndex::search sorts by (score DESC, node_id ASC)
        // which diverges from key order when nodes were not inserted in key-lex order.
        let mut results: Vec<(String, f64)> = self
            .fulltext
            .search(field, query, 0)
            .into_iter()
            .filter_map(|(id, score)| self.ids.key_of(id).map(|key| (key.to_string(), score)))
            .collect();
        results.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        results
    }

    /// [`search`](Self::search), stopping at the `k` best hits.
    ///
    /// Same ranking and the same deterministic tiebreak, but the index drops
    /// everything past `k` before any key is resolved, so a caller that wants
    /// the top few out of a field that matched thousands does not pay to
    /// materialise and re-sort the tail. `k == 0` means no limit, exactly as
    /// [`search`](Self::search) behaves.
    ///
    /// The BM25 scoring itself is not bounded by `k` — every candidate is
    /// scored either way — so this trims the resolve and the sort, not the
    /// search.
    pub fn search_top(&self, field: &str, query: &str, k: usize) -> Vec<(String, f64)> {
        // A tombstoned id resolves to nothing, so asking the index for exactly
        // `k` could return fewer. Over-fetching a little and truncating after
        // the filter keeps the count right without unbounding the call.
        let want = if k == 0 { 0 } else { k.saturating_mul(2) };
        let mut results: Vec<(String, f64)> = self
            .fulltext
            .search(field, query, want)
            .into_iter()
            .filter_map(|(id, score)| self.ids.key_of(id).map(|key| (key.to_string(), score)))
            .collect();
        results.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        if k > 0 {
            results.truncate(k);
        }
        results
    }

    /// Hybrid search: Reciprocal Rank Fusion (RRF) over fulltext + vector results.
    ///
    /// Takes up to `4*k` fulltext hits for `(text_field, query_text)` and up to
    /// `4*k` vector hits for `(vector_field, query_vec, min=0.0)`, then fuses
    /// them with RRF using a fixed constant of 60.
    ///
    /// ```text
    /// score(d) = Σ  1 / (60 + rank_i(d))    (rank 1-based per list)
    /// ```
    ///
    /// Returns the top `k` nodes by fused score, ties broken by node key
    /// ascending (deterministic).
    ///
    /// # Vector leg fallback
    ///
    /// When `query_vec` is empty the vector leg is skipped entirely and
    /// results are ranked by the text list alone through the same RRF path
    /// (each text result scores `1/(60 + rank)` from that single list).
    ///
    /// When `label` is `None`, the vector leg **always** returns empty results.
    /// Internally `label` is mapped to `""`, which does not match any rule-created
    /// HNSW index (all such indexes are keyed to a specific non-empty label), and
    /// the brute-force fallback finds no nodes with an empty label.  The fused
    /// ranking is therefore text-only in this case.
    pub fn search_hybrid(
        &self,
        text_field: &str,
        query_text: &str,
        vector_field: &str,
        query_vec: &[f64],
        label: Option<&str>,
        k: usize,
    ) -> Vec<(String, f64)> {
        self.search_hybrid_inner(
            text_field,
            query_text,
            vector_field,
            query_vec,
            label,
            k,
            None,
        )
    }

    /// [`search_hybrid`](Self::search_hybrid) with **each leg** filtered to the
    /// mask before the fusion.
    ///
    /// Filtering the fused list afterwards would quietly return fewer than `k`.
    /// Each leg over-fetches `4*k` candidates, so when the visible nodes rank
    /// below `4*k` hidden ones neither leg carries them into the fusion at all
    /// and the post-filter has nothing left to keep. Filtering first spends the
    /// `4*k` on **visible** hits, so a scoped call is as long as the corpus it
    /// can see allows.
    ///
    /// The ranks that enter RRF are therefore the ranks of the visible corpus,
    /// not the visible entries of the store-wide ranking. The constant stays 60
    /// and the tiebreak stays key-ascending.
    #[allow(clippy::too_many_arguments)]
    pub fn search_hybrid_scoped(
        &self,
        text_field: &str,
        query_text: &str,
        vector_field: &str,
        query_vec: &[f64],
        label: Option<&str>,
        k: usize,
        mask: &crate::mask::NodeMask,
    ) -> Vec<(String, f64)> {
        self.search_hybrid_inner(
            text_field,
            query_text,
            vector_field,
            query_vec,
            label,
            k,
            Some(mask),
        )
    }

    /// The body shared by [`search_hybrid`](Self::search_hybrid) and
    /// [`search_hybrid_scoped`](Self::search_hybrid_scoped). `mask = None` is
    /// the unscoped contract unchanged: the filter below is then a no-op and
    /// the vector leg is the same unmasked call it has always been.
    #[allow(clippy::too_many_arguments)]
    fn search_hybrid_inner(
        &self,
        text_field: &str,
        query_text: &str,
        vector_field: &str,
        query_vec: &[f64],
        label: Option<&str>,
        k: usize,
        mask: Option<&crate::mask::NodeMask>,
    ) -> Vec<(String, f64)> {
        use std::collections::HashMap;

        const RRF_K: f64 = 60.0;
        let pool = 4 * k;

        // Accumulate per-node RRF scores.
        let mut scores: HashMap<String, f64> = HashMap::new();

        // Text leg. The mask bites on the candidates, before `take(pool)`, so
        // the over-fetch is a budget of visible hits rather than one a hidden
        // prefix can exhaust.
        let text_hits = self.search(text_field, query_text);
        let visible_text = text_hits
            .into_iter()
            .filter(|(key, _count)| mask.is_none_or(|m| m.contains_node(self, key)));
        for (rank0, (key, _count)) in visible_text.take(pool).enumerate() {
            let rank = (rank0 + 1) as f64;
            *scores.entry(key).or_insert(0.0) += 1.0 / (RRF_K + rank);
        }

        // Vector leg (skipped when query_vec is empty). The masked variant
        // applies the mask before its own k-truncation, for the same reason.
        if !query_vec.is_empty() {
            // `ExactnessCaller::Hybrid`: the leg is the same one
            // `find_similar_vector_masked` runs, but the advice its warning
            // gives has to fit *this* signature, which has no `exact`.
            let vec_hits = self
                .find_similar_vector_as(
                    vector_field,
                    label,
                    query_vec,
                    pool,
                    0.0,
                    mask,
                    None,
                    false,
                    ExactnessCaller::Hybrid,
                )
                .expect("find_similar_vector_as is infallible without where_");
            for (rank0, (key, _sim)) in vec_hits.into_iter().enumerate() {
                let rank = (rank0 + 1) as f64;
                *scores.entry(key).or_insert(0.0) += 1.0 / (RRF_K + rank);
            }
        }

        // Sort: score DESC, then key ASC for deterministic tie-breaking.
        let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        ranked.truncate(k);
        ranked
    }

    /// For DST/testing: scratch BM25 search over live nodes without the index.
    /// Walks every live node, re-stems field tokens, computes corpus stats, and
    /// returns BM25-ranked results.
    ///
    /// The oracle: the ordered key list of `search(field, q)` must equal that of
    /// `scratch_search(field, q)` at every quiescent state.
    #[doc(hidden)]
    pub fn scratch_search(&self, field: &str, query: &str) -> Vec<(String, f64)> {
        use core_storage::fulltext::{parse_query, value_tokens_stemmed_with_positions};
        use std::collections::BTreeMap;

        let groups = parse_query(query);
        if groups.is_empty() {
            return vec![];
        }

        // --- Pass 1: collect all live indexed nodes with stemmed token data ---
        struct NodeData {
            key: String,
            /// stemmed_token → positions (sorted)
            tokens: BTreeMap<String, Vec<u32>>,
            dl: u32,
        }

        let mut nodes: Vec<NodeData> = Vec::new();
        for id in 0..self.ids.len() as u32 {
            let Some(key) = self.ids.key_of(id) else {
                continue;
            };
            let Some(&sym) = self.labels.get(id as usize) else {
                continue;
            };
            if sym == u32::MAX {
                continue;
            }
            let label = match self.syms.resolve(sym) {
                Some(l) => l,
                None => continue,
            };
            if !self.fulltext.is_enabled(label, field) {
                continue;
            }
            let Some(value) = self.props_view().get(id, field).map(|vr| vr.into_value()) else {
                continue;
            };
            // Use value_tokens_stemmed_with_positions so list elements are
            // separated by POSITION_GAP — identical to the index path, which
            // prevents phrase queries from matching across element boundaries.
            let stemmed_with_pos = match &value {
                Value::Str(_) | Value::List(_) => value_tokens_stemmed_with_positions(&value),
                _ => continue,
            };
            let dl = stemmed_with_pos.len() as u32;
            let mut tok_map: BTreeMap<String, Vec<u32>> = BTreeMap::new();
            for (tok, pos) in stemmed_with_pos {
                tok_map.entry(tok).or_default().push(pos);
            }
            nodes.push(NodeData {
                key: key.to_string(),
                tokens: tok_map,
                dl,
            });
        }

        if nodes.is_empty() {
            return vec![];
        }

        // --- BM25 corpus stats ---
        let n = nodes.len() as f64;
        let avg_dl: f64 = nodes.iter().map(|nd| nd.dl as f64).sum::<f64>() / n;
        // df per stemmed token across all live indexed nodes.
        let mut df_map: BTreeMap<&str, f64> = BTreeMap::new();
        for nd in &nodes {
            for tok in nd.tokens.keys() {
                *df_map.entry(tok.as_str()).or_insert(0.0) += 1.0;
            }
        }

        const K1: f64 = 1.2;
        const B: f64 = 0.75;

        // --- Pass 2: score each node against each OR-group ---
        let mut results: Vec<(String, f64)> = Vec::new();
        for nd in &nodes {
            let dl = nd.dl as f64;
            let mut total_score = 0.0f64;

            'group: for group in &groups {
                let mut group_score = 0.0f64;

                for term in group {
                    if term.negated {
                        // Negated: if doc has this stemmed token → group fails.
                        let present = if term.prefix {
                            nd.tokens.keys().any(|t| t.starts_with(term.token.as_str()))
                        } else {
                            nd.tokens.contains_key(term.token.as_str())
                        };
                        if present {
                            continue 'group;
                        }
                        continue;
                    }
                    if term.prefix {
                        // Prefix: sum BM25 for all matching stemmed tokens.
                        let mut prefix_matched = false;
                        for (tok, positions) in &nd.tokens {
                            if tok.starts_with(term.token.as_str()) {
                                let tf = positions.len() as f64;
                                let df = df_map.get(tok.as_str()).copied().unwrap_or(1.0);
                                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                                let tf_norm =
                                    tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avg_dl));
                                group_score += idf * tf_norm;
                                prefix_matched = true;
                            }
                        }
                        if !prefix_matched {
                            continue 'group;
                        }
                    } else {
                        // term.token is already stemmed by parse_query; use directly.
                        match nd.tokens.get(term.token.as_str()) {
                            None => continue 'group,
                            Some(positions) => {
                                let tf = positions.len() as f64;
                                let df = df_map.get(term.token.as_str()).copied().unwrap_or(1.0);
                                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                                let tf_norm =
                                    tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avg_dl));
                                group_score += idf * tf_norm;
                            }
                        }
                    }
                }

                if group_score > 0.0 {
                    total_score += group_score;
                }
            }

            if total_score > 0.0 {
                results.push((nd.key.clone(), total_score));
            }
        }

        results.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        results
    }

    /// Return the current view-maintained value of `view_prop` for node `key`.
    /// Equivalent to `get_prop` but documents that it reads a view-managed column.
    pub fn get_view_prop(&self, key: &str, view_prop: &str) -> Option<Value> {
        let id = self.ids.get(key)?;
        self.props_view()
            .get(id, view_prop)
            .map(|vr| vr.into_value())
    }

    /// For testing / DST oracle: scratch recompute of a view value for one node.
    ///
    /// Returns `None` if the node does not exist, the view does not exist, or
    /// the view has no result for the node (e.g. Avg with no qualifying neighbors).
    #[doc(hidden)]
    pub fn scratch_view_value(&self, key: &str, view_name: &str) -> Option<Value> {
        let node = self.ids.get(key)?;
        let def = self.view_store.views().find(|v| v.name == view_name)?;
        // Use TopologyView so that NeighborAgg sees base + overlay edges
        // without materialising a temporary Topology (I1).
        let topo_view = self.topo_view();
        core_rules::views::compute_view_value(
            def,
            node,
            self.props_view(),
            &topo_view,
            &self.ids,
            &self.syms,
            &self.labels,
        )
    }

    // -----------------------------------------------------------------------
    // Graph algorithm API
    // -----------------------------------------------------------------------

    /// Run PageRank over the unified topology (manual + derived edges).
    ///
    /// Returns a [`PageRankReport`] with scores sorted descending (ties: key
    /// ascending).  Set `config.edge_type` to restrict to one edge type.
    /// `config.converged` is `true` only when the power iteration converged
    /// within `config.max_iters` and within any time budget.
    pub fn pagerank(&self, config: &crate::algo::PageRankConfig) -> crate::algo::PageRankReport {
        crate::algo::pagerank(&self.view(), config)
    }

    /// Weakly-connected components over the unified topology (treated as
    /// undirected regardless of how edges were inserted).
    ///
    /// Component IDs are the key of the smallest member in the component
    /// (deterministic).  Result sorted by (component_id, key).
    pub fn connected_components(&self, config: &crate::algo::WccConfig) -> crate::algo::WccReport {
        crate::algo::wcc(&self.view(), config)
    }

    /// Degree centrality for every live node.
    ///
    /// `direction`: `AlgoDir::Out` = out-degree, `AlgoDir::In` = in-degree,
    /// `AlgoDir::Both` = out + in (total directed degree).
    ///
    /// For one-shot ranking use this; for a live property updated on every
    /// write, create a Degree materialized view instead (see `docs/site/algorithms.md`).
    pub fn degree_centrality(
        &self,
        config: &crate::algo::DegreeConfig,
    ) -> crate::algo::DegreeReport {
        crate::algo::degree_centrality(&self.view(), config)
    }

    /// Louvain community detection over the unified topology (undirected).
    ///
    /// See [`crate::algo::LouvainConfig`] for edge-type/weight/label
    /// restriction and [`crate::algo::CommunityReport`] for the shape of the
    /// result (communities sorted size-desc, then smallest member key asc).
    pub fn communities(&self, config: &crate::algo::LouvainConfig) -> crate::algo::CommunityReport {
        let topo = build_topo_view(&self.topo, &self.base);
        let edge_props = self.edge_props_view();
        crate::algo::louvain(
            &topo,
            &self.ids,
            &self.syms,
            &self.labels,
            &edge_props,
            config,
        )
    }

    /// Write a vector of `(node_key, score)` pairs as `prop_name` on each node,
    /// atomically via a single write-batch (one WAL frame, one fsync).
    ///
    /// # Errors
    /// - [`GraphError::ReadOnly`]: called on an as-of instance.
    /// - [`GraphError::RuleInvalid`]: `prop_name` is managed by an existing view
    ///   (collision check mirrors `create_view`).
    /// - [`GraphError::KeyNotFound`]: a key in `scores` does not exist as a live node.
    pub fn write_scores(&mut self, prop_name: &str, scores: &[(String, f64)]) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // Collision check: refuse if prop_name is view-managed.
        if let Some(view_name) = self.view_store.view_for_prop(prop_name) {
            return Err(GraphError::RuleInvalid {
                detail: format!(
                    "prop {:?} is managed by view {:?} and cannot be written as scores",
                    prop_name, view_name
                ),
            });
        }
        // Refuse if prop_name is a view name itself (confusing namespace collision).
        if self.view_store.has_view(prop_name) {
            return Err(GraphError::RuleInvalid {
                detail: format!(
                    "prop_name {:?} collides with an existing view name",
                    prop_name
                ),
            });
        }
        // Write all scores in a single crash-atomic batch.
        self.write_batch(|b| {
            for (key, score) in scores {
                b.set_prop(key, prop_name, Value::Float(*score));
            }
        })?;
        Ok(())
    }

    /// Return the value of `field` for the node with key `key`, or `None` if
    /// the node or field is absent.  Reads through the overlay-over-base
    /// `ColumnsView`, materialising base values on demand (zero heap cost for
    /// overlay hits; one clone per base hit).
    pub fn get_prop(&self, key: &str, field: &str) -> Option<Value> {
        let id = self.ids.get(key)?;
        self.props_view().get(id, field).map(|vr| vr.into_value())
    }

    pub fn has_node(&self, key: &str) -> bool {
        self.ids.get(key).is_some()
    }

    /// One human line for a node: the first of `text`, `summary` or `name` it
    /// carries, truncated. Used by the recall digest.
    pub fn node_summary_line(&self, key: &str) -> Option<String> {
        for field in ["text", "summary", "name"] {
            if let Some(Value::Str(s)) = self.get_prop(key, field) {
                let s = s.trim();
                if !s.is_empty() {
                    return Some(s.chars().take(120).collect());
                }
            }
        }
        None
    }

    /// Borrow the raw id map. Used by `NodeMask::from_keys` to resolve keys.
    pub(crate) fn ids(&self) -> &IdMap {
        &self.ids
    }

    // -----------------------------------------------------------------------
    // Namespaces
    // -----------------------------------------------------------------------

    /// The index `name` already has in `ns_names`, if any.
    fn ns_index_of(&self, name: &str) -> Option<u32> {
        self.ns_names
            .iter()
            .position(|n| n == name)
            .map(|i| i as u32)
    }

    /// The index for `name`, appending it to `ns_names` when it is new.
    ///
    /// The table holds one entry per distinct namespace in the store — a
    /// tenant count, not a node count — so the linear scan is cheaper than a
    /// map and keeps `namespaces()` allocation-free of a second index.
    fn ns_index_for(&mut self, name: &str) -> u32 {
        match self.ns_index_of(name) {
            Some(i) => i,
            None => {
                self.ns_names.push(name.to_string());
                (self.ns_names.len() - 1) as u32
            }
        }
    }

    /// The namespace name at `idx`, or [`NS_DEFAULT`] for an index this handle
    /// does not know (unreachable; the default is the narrowing answer).
    fn ns_name(&self, idx: u32) -> &str {
        self.ns_names
            .get(idx as usize)
            .map(String::as_str)
            .unwrap_or(NS_DEFAULT)
    }

    /// The namespace index of dense node `id`, defaulting for an id with no
    /// entry (a node inserted before this handle rebuilt the array cannot
    /// exist: every insert path maintains it).
    fn node_ns_idx(&self, id: u32) -> u32 {
        self.node_ns
            .get(id as usize)
            .copied()
            .unwrap_or(NS_DEFAULT_IDX)
    }

    /// File node `id` under namespace `name`, growing `node_ns` as `labels`
    /// grows. Called from `apply` for every node insert, live and replayed.
    fn set_node_ns(&mut self, id: u32, name: &str) {
        let idx = if name == NS_DEFAULT {
            NS_DEFAULT_IDX
        } else {
            self.ns_index_for(name)
        };
        if self.node_ns.len() <= id as usize {
            self.node_ns.resize(id as usize + 1, NS_DEFAULT_IDX);
        }
        self.node_ns[id as usize] = idx;
    }

    /// Rebuild `node_ns` from the `ns` column — one pass, at the end of an
    /// open or a reload, after the snapshot is restored and the WAL replayed.
    ///
    /// A store with no `ns` column reads nothing: the column-name check fails
    /// and the vector is filled with one constant.
    fn rebuild_node_ns(&mut self) {
        let total = self.ids.len();
        self.ns_names.truncate(1);
        self.node_ns.clear();
        self.node_ns.resize(total, NS_DEFAULT_IDX);
        let has_ns_column = {
            let cv = self.props_view();
            cv.field_names().iter().any(|f| f == NS_PROP)
        };
        if !has_ns_column {
            return;
        }
        // Collected first so the props view is released before `ns_index_for`
        // takes `&mut self`.
        let named: Vec<(u32, String)> = {
            let cv = self.props_view();
            (0..total as u32)
                .filter_map(|id| match cv.get(id, NS_PROP).map(|vr| vr.into_value()) {
                    Some(Value::Str(s)) if s != NS_DEFAULT => Some((id, s)),
                    _ => None,
                })
                .collect()
        };
        for (id, name) in named {
            let idx = self.ns_index_for(&name);
            self.node_ns[id as usize] = idx;
        }
    }

    /// Every namespace with at least one live node, in name order.
    ///
    /// `["default"]` on any store that has never named a namespace, including
    /// an empty one: a store is always at least its default namespace.
    pub fn namespaces(&self) -> Vec<String> {
        let mut out: BTreeSet<&str> = BTreeSet::new();
        out.insert(NS_DEFAULT);
        for (id, &idx) in self.node_ns.iter().enumerate() {
            if idx == NS_DEFAULT_IDX || !self.is_live_node(id as u32) {
                continue;
            }
            out.insert(self.ns_name(idx));
        }
        out.into_iter().map(str::to_string).collect()
    }

    /// The namespace of `key`, or `None` when the key names no live node.
    pub fn namespace_of(&self, key: &str) -> Option<String> {
        let id = self.ids.get(key)?;
        if !self.is_live_node(id) {
            return None;
        }
        Some(self.ns_name(self.node_ns_idx(id)).to_string())
    }

    /// Every live node in `namespace`, as a visibility mask.
    ///
    /// Built off `node_ns` on whichever handle this is, so on a temporal handle
    /// it is the namespace's membership at that commit. A name no node uses
    /// gives an empty mask — a namespace scope never widens.
    pub fn mask_for_namespace(&self, namespace: &str) -> crate::mask::NodeMask {
        let Some(idx) = self.ns_index_of(namespace) else {
            return crate::mask::NodeMask::from_ids(std::collections::HashSet::new());
        };
        let visible: std::collections::HashSet<u32> = (0..self.ids.len() as u32)
            .filter(|&id| self.node_ns_idx(id) == idx && self.is_live_node(id))
            .collect();
        crate::mask::NodeMask::from_ids(visible)
    }

    /// Live-node test used by the namespace accessors: a deleted node keeps its
    /// dense id and its `node_ns` slot, and the label sentinel is what marks it
    /// gone — the same test `mask_for_role`'s label leg applies implicitly.
    fn is_live_node(&self, id: u32) -> bool {
        self.labels
            .get(id as usize)
            .is_some_and(|&sym| sym != u32::MAX)
            && self.ids.key_of(id).is_some()
    }

    /// Per-namespace live node counts for [`Stats`], in name order.
    fn namespace_stats(&self) -> Vec<NamespaceStats> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        counts.insert(NS_DEFAULT, 0);
        for id in 0..self.ids.len() as u32 {
            if !self.is_live_node(id) {
                continue;
            }
            *counts
                .entry(self.ns_name(self.node_ns_idx(id)))
                .or_insert(0) += 1;
        }
        counts
            .into_iter()
            .filter(|&(name, n)| n > 0 || name == NS_DEFAULT)
            .map(|(name, nodes_live)| NamespaceStats {
                name: name.to_string(),
                nodes_live,
            })
            .collect()
    }

    /// The namespace a create-class op would put its node in: the `ns` entry of
    /// the props it carries, normalised, with absent meaning [`NS_DEFAULT`].
    fn created_namespace<'a>(key: &str, props: &'a [(String, Value)]) -> Result<&'a str> {
        Ok(namespace_of_value(Self::sole_ns_entry(key, props)?))
    }

    /// The one `ns` entry in a node's props, or `None` when it carries none.
    ///
    /// A props list naming `ns` twice is refused. Without that refusal the
    /// write path and the authorisation path can read the same list
    /// differently — one taking the first entry, the other the last — and
    /// `CREATE (n:L {ns: 'mine', ns: 'theirs'})` lands a node in a namespace
    /// the role was checked against the other of. One entry is the only shape
    /// where "the node's namespace" is a single fact, so it is the only shape
    /// accepted, and every reader of it agrees by construction.
    fn sole_ns_entry<'a>(key: &str, props: &'a [(String, Value)]) -> Result<Option<&'a Value>> {
        let mut found: Option<&'a Value> = None;
        for (field, value) in props {
            if field != NS_PROP {
                continue;
            }
            if found.is_some() {
                return Err(GraphError::RuleInvalid {
                    detail: format!(
                        "node {key}: {NS_PROP} is given more than once; a node has exactly \
                         one namespace"
                    ),
                });
            }
            found = Some(value);
        }
        Ok(found)
    }

    /// The definition of the role a write authorisation names.
    ///
    /// `None` when `roles.json` was corrupt at open or the role has since been
    /// removed — neither can reach a write, because the authorisation carries a
    /// mask `mask_for_role` already resolved for that name.
    fn role_def_for(&self, role: &str) -> Option<&RoleDef> {
        self.roles.as_ref()?.iter().find(|r| r.name == role)
    }

    /// Validate the `ns` entry of a node's props and drop an explicit default.
    ///
    /// Runs on the write path only (see `rewrite_wal_dense`), never on replay:
    /// a record that reached the WAL was already accepted here.
    fn normalise_insert_ns(
        key: &str,
        props: Vec<(String, Value)>,
    ) -> Result<(Vec<(String, Value)>, String)> {
        // One `ns` or none: this is where that is enforced, so every later
        // reader of the list — the authorisation gate, the two `apply` arms,
        // `node_ns` — is looking at a single entry and cannot disagree about
        // which one counts.
        Self::sole_ns_entry(key, &props)?;
        let mut name = NS_DEFAULT.to_string();
        let mut out = Vec::with_capacity(props.len());
        for (field, value) in props {
            if field != NS_PROP {
                out.push((field, value));
                continue;
            }
            let Value::Str(ref s) = value else {
                return Err(GraphError::RuleInvalid {
                    detail: format!(
                        "node {key}: {NS_PROP} must be a string naming a namespace, \
                         got {value:?}"
                    ),
                });
            };
            if !valid_namespace(s) {
                return Err(GraphError::RuleInvalid {
                    detail: format!(
                        "node {key}: {s:?} is not a valid namespace name — 1 to {NS_MAX_LEN} \
                         characters of [A-Za-z0-9_.-]"
                    ),
                });
            }
            name = s.clone();
            // An explicit default stores nothing, so a single-tenant store
            // never grows an `ns` column.
            if name != NS_DEFAULT {
                out.push((field, value));
            }
        }
        Ok((out, name))
    }

    // -----------------------------------------------------------------------
    // RBAC role resolution
    // -----------------------------------------------------------------------

    /// Parse `roles.json` bytes from `fs`.
    ///
    /// Return values:
    ///   `Ok(Some(roles))` — file absent (returns `vec![]`) **or** file present
    ///                       and valid; in both cases `mask_for_role` uses the
    ///                       list normally (an absent file means no roles defined).
    ///   `Ok(None)`        — file present but corrupt or unrecognised version
    ///                       → poisoned state; `mask_for_role` returns `Err` for
    ///                       any role name until the file is fixed and the DB
    ///                       re-opened (or `apply_schema` is called to repair it).
    ///
    /// Note: `None` signals corruption, not absence — the opposite of what an
    /// optional "file missing" convention would suggest.  The open path stores
    /// this result on `db.roles` directly.
    fn load_roles_from_fs(fs: &F) -> Result<Option<Vec<RoleDef>>> {
        let bytes = fs.read(FileId::Roles).map_err(GraphError::Io)?;
        if bytes.is_empty() {
            // Empty bytes means either the file is absent or zero-byte — both
            // are treated identically as "no roles defined".  A zero-byte
            // roles.json does NOT widen access: an absent file and a zero-byte
            // file both resolve to an empty role list (sees nothing by default).
            return Ok(Some(vec![]));
        }
        match serde_json::from_slice::<RolesFile>(&bytes) {
            Ok(f) if matches!(f.version, 1..=4) => Ok(Some(f.roles)),
            // Corrupt or unrecognised version (>4): poison the roles state.
            // Never widen: a version this binary does not know may carry a
            // narrowing this binary would not apply.
            _ => Ok(None),
        }
    }

    /// Resolve a role to a node-visibility mask against the current graph state.
    ///
    /// Returns `Err` when:
    /// - `roles.json` was present but corrupt at open (poisoned state), or
    /// - `role` does not match any defined role name.
    ///
    /// The mask union is: explicit `keys` (unknown keys silently ignored) plus
    /// all live nodes carrying any label in `labels` that also pass the role's
    /// [`visible_where`](crate::roles::RoleDef::visible_where) predicate, if it
    /// has one.  Label resolution is live — new nodes of an allowed label are
    /// visible without re-applying the schema, and a property edited out of the
    /// predicate takes its node out of the mask on the next read.  An empty
    /// union = empty mask = sees nothing.
    ///
    /// This is the one resolver every read path calls, live and as-of alike, so
    /// the predicate applies everywhere at once.  On an as-of handle the role
    /// *definition* is the current one and the graph is the historical one: the
    /// predicate is evaluated against the property values at the commit being
    /// read.
    ///
    /// The result is memoised per `(role, commit_seq)`, so a scoped reader
    /// between two writes resolves the role once.  See
    /// [`RoleMaskCache`](crate::mask::RoleMaskCache) for why that cannot go
    /// stale.
    pub fn mask_for_role(&self, role: &str) -> Result<crate::mask::NodeMask> {
        self.role_masks
            .get_or_build(role, self.commit_seq, || self.build_mask_for_role(role))
            .map(|m| (*m).clone())
    }

    /// The mask an [`AsOfScope`] names, resolved against this handle.
    ///
    /// Shared by [`GraphDb::query_at_scoped`] and
    /// [`GraphDb::query_at_scoped_in_namespace`] so one scope resolves one way
    /// however the namespace leg is added.
    fn mask_at_scope(&self, scope: AsOfScope<'_>) -> Result<crate::mask::NodeMask> {
        // One resolver answers "what may this role see" — `mask_for_role` — and
        // it runs against this handle, so on a temporal one the answer is the
        // as-of one.
        Ok(match scope {
            AsOfScope::Role(role) => self.mask_for_role(role)?,
            AsOfScope::Keys(keys) => {
                crate::mask::NodeMask::from_keys(self, keys.iter().map(String::as_str))
            }
            AsOfScope::RoleAndKeys(role, keys) => {
                self.mask_for_role(role)?
                    .intersect(&crate::mask::NodeMask::from_keys(
                        self,
                        keys.iter().map(String::as_str),
                    ))
            }
            AsOfScope::Namespace(namespace) => self.mask_for_namespace(namespace),
        })
    }

    /// Resolve `role` against the current graph, ignoring the memo.
    fn build_mask_for_role(&self, role: &str) -> Result<crate::mask::NodeMask> {
        let roles = self.roles.as_ref().ok_or_else(roles_poisoned)?;
        let def = roles
            .iter()
            .find(|r| r.name == role)
            .ok_or_else(|| GraphError::KeyNotFound {
                key: format!("role:{role}"),
            })?;

        let mut visible = std::collections::HashSet::new();

        // Key leg: resolve explicit keys to dense ids (unknown keys ignored).
        // An administrative grant, never narrowed by the predicate.
        for key in &def.keys {
            if let Some(id) = self.ids.get(key) {
                visible.insert(id);
            }
        }

        // Label leg: live scan — iterate labels vec for matching symbol, and
        // when the role carries a predicate, test the property as well.  The
        // property comes from the store's own merged view (overlay over the
        // mmap'd base), so an as-of handle reads the values of its own commit.
        let props = def.visible_where.as_ref().map(|_| self.props_view());
        for label_name in &def.labels {
            if let Some(sym) = self.syms.get(label_name) {
                for (i, &s) in self.labels.iter().enumerate() {
                    if s != sym {
                        continue;
                    }
                    let id = i as u32;
                    match (&def.visible_where, &props) {
                        (Some(pred), Some(view)) => {
                            let value = view.get(id, &pred.field).map(|vr| vr.into_value());
                            if pred.holds(value.as_ref()) {
                                visible.insert(id);
                            }
                        }
                        _ => {
                            visible.insert(id);
                        }
                    }
                }
            }
        }

        // Namespace leg: an intersection over the whole union, the key leg
        // included. A namespace is a tenancy boundary, so a key naming a node in
        // another tenant's namespace is not an administrative grant — and
        // `apply_schema` has already refused that role, so this only has to be
        // right about the node that moved into existence afterwards.
        if def.namespaces.is_some() {
            visible.retain(|&id| def.sees_namespace(self.ns_name(self.node_ns_idx(id))));
        }

        Ok(crate::mask::NodeMask::from_ids(visible))
    }

    /// Return the current list of role definitions.
    ///
    /// Returns an empty list when no roles are defined or when `roles.json`
    /// was corrupt at open (check [`mask_for_role`](Self::mask_for_role) for
    /// the fail-loud error in that case, or call
    /// [`roles_checked`](Self::roles_checked), which is this readout with that
    /// error in it).
    pub fn roles(&self) -> Vec<RoleDef> {
        self.roles.as_deref().unwrap_or(&[]).to_vec()
    }

    /// The role definitions, or the poison error when `roles.json` was corrupt
    /// at open.
    ///
    /// [`roles`](Self::roles) answers `[]` both for a store that defines no
    /// roles and for one whose sidecar did not parse, and a caller validating a
    /// role name at boot cannot tell those apart. The wrong reading of the pair
    /// is the dangerous one: a store with no roles at all is an unrestricted
    /// store, so a poisoned file would read as "nothing is restricted here".
    ///
    /// This is the same answer, for the same cause, that
    /// [`mask_for_role`](Self::mask_for_role) gives on the first read.
    pub fn roles_checked(&self) -> Result<Vec<RoleDef>> {
        match self.roles.as_deref() {
            Some(roles) => Ok(roles.to_vec()),
            None => Err(roles_poisoned()),
        }
    }

    // ── Role-scoped write authz ───────────────────────────────────────────────

    /// Execute `ops` with optional role-scoped write authorization.
    ///
    /// - `None` → full authority, identical to [`write_batch`](Self::write_batch)
    ///   (zero-cost bypass of all authz checks).
    /// - `Some(authz)` → the decision table is evaluated per-op BEFORE any WAL
    ///   record is built.  A denial returns an error with no WAL frame written
    ///   (all-or-nothing at the authz boundary, then at the MutPreview boundary).
    ///
    /// See the plan's "authz decision table" section for the full semantics.
    pub fn write_batch_authz(
        &mut self,
        authz: Option<&WriteAuthz>,
        ops: Vec<BatchOp>,
    ) -> Result<(usize, usize)> {
        // Thread authz as a direct parameter — never touches pending_write_authz.
        self.commit_logged_batch(ops, None, authz.cloned())
            .map(inserted_pair)
    }

    /// Execute a Cypher write statement with role-scoped write authorization.
    ///
    /// Resolves scope + mask from `self.roles` inside the call (same write-guard
    /// lifetime as execution, satisfying §5 lock discipline).  The resolved
    /// `WriteAuthz` is stored as `pending_write_authz` for the duration of the
    /// call so that all inner `batch.commit()` calls are authz-checked.
    ///
    /// MERGE is handled specially: the MERGE scope precondition (§3.3) is
    /// checked in `exec_merge` BEFORE `has_node` to close the §6.2
    /// timing-oracle item (hidden ≡ absent for unscoped roles).
    ///
    /// Roles with `write: None` (v1 behavior) → `RoleWriteDenied` with
    /// "this endpoint is not permitted".
    pub fn query_write_authz(
        &mut self,
        role: &str,
        cypher: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // Resolve scope (fails fast if role has no write scope).
        // write:None → byte-identical v1 blanket-403 body (plan §v1-sidecar mandate).
        let scope =
            {
                let roles = self.roles.as_deref().ok_or_else(|| GraphError::Corrupt {
                    detail: "roles.json was corrupt at open; re-open to restore role access".into(),
                })?;
                let def = roles.iter().find(|r| r.name == role).ok_or_else(|| {
                    GraphError::KeyNotFound {
                        key: format!("role:{role}"),
                    }
                })?;
                def.write
                    .clone()
                    .ok_or_else(|| GraphError::RoleWriteDenied {
                        reason: "role-bound token: writes are not permitted".into(),
                    })?
            };
        // Resolve mask inside the call (same guard, §5 coherence).
        let mask = self.mask_for_role(role)?;
        self.pending_write_authz = Some(WriteAuthz {
            role: role.into(),
            scope,
            mask,
        });
        // RAII guard: always clears pending_write_authz on scope exit, including
        // on panic or early-return, mirroring the RestoreEmitDeltas precedent.
        struct ClearPendingAuthzOnDrop(*mut Option<WriteAuthz>);
        impl Drop for ClearPendingAuthzOnDrop {
            fn drop(&mut self) {
                // SAFETY: pointer into the owning GraphDb; guard is dropped
                // within this function's frame before it returns.
                unsafe { *self.0 = None };
            }
        }
        // SAFETY: raw pointer into self; guard dropped before this fn returns.
        let _authz_guard = ClearPendingAuthzOnDrop(&mut self.pending_write_authz as *mut _);
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        // A role's write scope names labels and edge types; the schema is
        // outside every scope, so a role never changes it. `SHOW` is a read.
        if let Some(stmt) = ddl_statement(&tokens)? {
            if !stmt.is_show() {
                return Err(ddl::schema_change_for_a_role());
            }
            let authz = self
                .pending_write_authz
                .as_ref()
                .expect("set above for the length of this call");
            return Ok(self.schema_listing().show_visible(
                &stmt,
                &self.view(),
                &authz.mask.visible,
            ));
        }
        // `CALL` is a read: it sees what the role's mask lets through.
        if let Some(call) = call_statement(&tokens)? {
            let authz = self
                .pending_write_authz
                .as_ref()
                .expect("set above for the length of this call");
            return run_call(
                &call,
                &self.view_masked(&authz.mask),
                &self.schema_listing(),
                params,
            );
        }
        let stmt = parse_write(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        self.exec_write_stmt(stmt, params)
    }

    /// Execute `ops` with optional role-scoped write authorization, suppressing
    /// fsync (for use inside the group-commit drain thread, which performs one
    /// group fsync after releasing the write lock).
    ///
    /// Identical to [`write_batch_authz`] except the fsync policy is temporarily
    /// forced to `Relaxed` for the duration of the call, matching the drain-thread
    /// contract established by [`commit_batch_nosync`].
    pub(crate) fn write_batch_authz_nosync(
        &mut self,
        authz: Option<&WriteAuthz>,
        ops: Vec<BatchOp>,
    ) -> Result<(usize, usize)> {
        let saved = self.fsync;
        struct RestoreFsync(*mut FsyncPolicy, FsyncPolicy);
        impl Drop for RestoreFsync {
            fn drop(&mut self) {
                // SAFETY: pointer into the owning GraphDb; guard is dropped
                // within the enclosing function's frame before it returns.
                unsafe { *self.0 = self.1 };
            }
        }
        // SAFETY: raw pointer into self; guard dropped before this fn returns.
        let _g = RestoreFsync(&mut self.fsync as *mut FsyncPolicy, saved);
        self.fsync = FsyncPolicy::Relaxed;
        self.commit_logged_batch(ops, None, authz.cloned())
            .map(inserted_pair)
    }

    /// Execute a `/ingest` request with role-scoped write authorization.
    ///
    /// Resolves the role's `WriteScope` and `NodeMask` inside this call (same
    /// write-guard lifetime as the mutation, satisfying §5 lock discipline).
    /// Sets `pending_write_authz` for the duration of the call so that the
    /// `commit_ingest` → `commit_logged_batch` path picks up the authz context
    /// and evaluates the decision table per-op before any WAL write.
    ///
    /// §7.3: roles with empty `create_labels` will see every `InsertNode` op
    /// denied by the decision table with the appropriate §4.3 scope reason;
    /// no special HTTP-layer check is needed.
    ///
    /// Roles with `write: None` return `RoleWriteDenied` with
    /// "writes are not permitted" (byte-identical to v1 blanket 403).
    pub fn ingest_with_edges_authz(
        &mut self,
        role: &str,
        label: &str,
        rows: Vec<std::collections::BTreeMap<String, Value>>,
        opts: &crate::ingest::IngestOptions,
        edges: &[(String, String, String)],
    ) -> Result<crate::ingest::IngestReport> {
        // Resolve scope (fails fast if role has no write scope).
        // write:None → byte-identical v1 blanket-403 body (plan §v1-sidecar mandate).
        let scope =
            {
                let roles = self.roles.as_deref().ok_or_else(|| GraphError::Corrupt {
                    detail: "roles.json was corrupt at open; re-open to restore role access".into(),
                })?;
                let def = roles.iter().find(|r| r.name == role).ok_or_else(|| {
                    GraphError::KeyNotFound {
                        key: format!("role:{role}"),
                    }
                })?;
                def.write
                    .clone()
                    .ok_or_else(|| GraphError::RoleWriteDenied {
                        reason: "role-bound token: writes are not permitted".into(),
                    })?
            };
        let mask = self.mask_for_role(role)?;
        self.pending_write_authz = Some(WriteAuthz {
            role: role.into(),
            scope,
            mask,
        });
        // RAII guard: always clears pending_write_authz on scope exit, including
        // on panic or early-return, mirroring the RestoreEmitDeltas precedent.
        struct ClearPendingAuthzOnDrop(*mut Option<WriteAuthz>);
        impl Drop for ClearPendingAuthzOnDrop {
            fn drop(&mut self) {
                // SAFETY: pointer into the owning GraphDb; guard is dropped
                // within this function's frame before it returns.
                unsafe { *self.0 = None };
            }
        }
        // SAFETY: raw pointer into self; guard dropped before this fn returns.
        let _authz_guard = ClearPendingAuthzOnDrop(&mut self.pending_write_authz as *mut _);
        self.ingest_with_edges(label, rows, opts, edges)
    }

    /// Evaluate the write-authz decision table for one `BatchOp`.
    ///
    /// Called by `commit_logged_batch` for each op when `pending_write_authz`
    /// is `Some`, BEFORE MutPreview.  A denial returns an error immediately;
    /// the remaining ops are not evaluated and no WAL frame is written.
    ///
    /// `batch_created` carries the key→label pairs of nodes that earlier ops in
    /// THIS batch will create.  Used by `InsertEdgeUpsert` to count same-batch
    /// placeholder nodes as visible (spec: "a placeholder endpoint the SAME
    /// batch creates counts as visible if its label passed the create-class gate").
    fn check_single_op_authz(
        &self,
        authz: &WriteAuthz,
        op: &BatchOp,
        batch_created: &BTreeMap<String, String>,
    ) -> Result<()> {
        // Helper: 3-way node status under the authz mask.
        //
        // Batch-created nodes (from earlier InsertNode in THIS batch) are treated
        // as Visible with their recorded label — their create gate already passed
        // and they are not yet in self.ids (not committed).  This fixes the
        // MERGE+ON CREATE SET case where InsertNode + SetProp arrive together:
        // the SetProp must not see the node as Absent.
        let node_status = |key: &str| -> NodeAuthzStatus {
            if let Some(label) = batch_created.get(key) {
                return NodeAuthzStatus::Visible(label.clone());
            }
            match self.ids.get(key) {
                None => NodeAuthzStatus::Absent,
                Some(id) if !authz.mask.contains_id(id) => NodeAuthzStatus::Hidden,
                Some(id) => {
                    let label = self
                        .labels
                        .get(id as usize)
                        .and_then(|&sym| {
                            if sym == u32::MAX {
                                None
                            } else {
                                self.syms.resolve(sym).map(str::to_string)
                            }
                        })
                        .unwrap_or_default();
                    NodeAuthzStatus::Visible(label)
                }
            }
        };

        // Helper: is an InsertEdgeUpsert endpoint visible?
        // A same-batch placeholder counts as visible if its label passed
        // the create-class gate (spec "upsert placeholder-counts-as-visible").
        let upsert_ep_visible = |ep_key: &str, placeholder_label: &str| -> bool {
            // In store and visible?
            if let Some(id) = self.ids.get(ep_key) {
                return authz.mask.contains_id(id);
            }
            // Created by an earlier op in this batch?
            if let Some(created_label) = batch_created.get(ep_key) {
                return authz.scope.create_labels.contains(created_label);
            }
            // Will be created by THIS InsertEdgeUpsert: placeholder_label
            // must pass the create-class gate.
            authz
                .scope
                .create_labels
                .contains(&placeholder_label.to_string())
        };

        match op {
            // RenameNode / CreateRule / DeleteRule: defense-in-depth gate.
            // These ops are never routed to role-scoped paths by the HTTP layer,
            // but we 403 them here to close any future bypass route.
            //
            // InsertNodeOnConflict joins them: it is reachable only from the
            // embedded Python binding, which has no role token, and `Replace`
            // is a create and an update at once. Rather than split the decision
            // table for an op no role-scoped path constructs, refuse it — a
            // role-scoped caller writes through the ops that are already in the
            // table.
            BatchOp::RenameNode { .. }
            | BatchOp::CreateRule(_)
            | BatchOp::DeleteRule { .. }
            | BatchOp::InsertNodeOnConflict { .. } => {
                return Err(GraphError::RoleWriteDenied {
                    reason: "role-bound token: this endpoint is not permitted".into(),
                });
            }

            // ── CREATE-class: InsertNode ─────────────────────────────────────
            //
            // Decision table row 1 (scope-before-lookup): check label in
            // create_labels BEFORE any key lookup.  This is the structural
            // closure of the §6.2 timing-oracle item — the denial fires even
            // when the store is EMPTY (see test_create_scope_denied_empty_store).
            BatchOp::InsertNode { label, key, props } => {
                if !authz.scope.create_labels.contains(label) {
                    return Err(GraphError::RoleWriteDenied {
                        reason: format!(
                            "role-bound token: label '{}' not in write scope (create_labels)",
                            label
                        ),
                    });
                }
                // A role bound to namespaces may only create inside them. The
                // never-widen rule is about what a write makes visible to *any*
                // party, not only to the writer: a node this role could never
                // read back is a write into somebody else's tenancy. Also a
                // scope check, so it runs before the key lookup — it discloses
                // nothing about the store. Covers Cypher `CREATE` and the node
                // `MERGE` creates, both of which arrive as this op.
                // Resolved before the role lookup so a props list naming `ns`
                // twice is refused for every role, scoped or not: it is the same
                // malformed write the seam refuses, and leaving it to the seam
                // would mean the gate had already read one of the two.
                let target = Self::created_namespace(key, props)?;
                if let Some(def) = self.role_def_for(&authz.role) {
                    if !def.sees_namespace(target) {
                        return Err(GraphError::RoleWriteDenied {
                            reason: format!(
                                "role-bound token: namespace '{target}' not in the role's \
                                 namespaces"
                            ),
                        });
                    }
                }
                // Row 2/3: key lookup.
                match self.ids.get(key.as_str()) {
                    Some(id) if authz.mask.contains_id(id) => {
                        // Visible: DuplicateKey — let MutPreview handle this.
                    }
                    Some(_) => {
                        // Hidden: indistinguishable from absent to the role.
                        return Err(GraphError::RoleWriteDenied {
                            reason: "role-bound token: target node not visible".into(),
                        });
                    }
                    None => {
                        // Absent: proceed (create).
                    }
                }
            }

            // ── UPDATE-class: SetProp, RemoveProp ────────────────────────────
            BatchOp::SetProp { key, .. } | BatchOp::RemoveProp { key, .. } => {
                if batch_created.contains_key(key.as_str()) {
                    // Batch-created node: create gate already passed this batch.
                    // Updating it in the same batch is always allowed, regardless
                    // of update_labels (ruling §3.5: "writer just created it").
                } else {
                    let label = match node_status(key) {
                        NodeAuthzStatus::Visible(lbl) => lbl,
                        _ => {
                            return Err(GraphError::RoleWriteDenied {
                                reason: "role-bound token: target node not visible".into(),
                            });
                        }
                    };
                    if !authz.scope.update_labels.contains(&label) {
                        return Err(GraphError::RoleWriteDenied {
                            reason: format!(
                                "role-bound token: label '{}' not in write scope (update_labels)",
                                label
                            ),
                        });
                    }
                }
            }

            // ── DELETE-class: DeleteNode ─────────────────────────────────────
            BatchOp::DeleteNode { key } => {
                let label = match node_status(key) {
                    NodeAuthzStatus::Visible(lbl) => lbl,
                    _ => {
                        return Err(GraphError::RoleWriteDenied {
                            reason: "role-bound token: target node not visible".into(),
                        });
                    }
                };
                if !authz.scope.delete_labels.contains(&label) {
                    return Err(GraphError::RoleWriteDenied {
                        reason: format!(
                            "role-bound token: label '{}' not in write scope (delete_labels)",
                            label
                        ),
                    });
                }
            }

            // ── DELETE-class: DeleteEdge ─────────────────────────────────────
            //
            // Derived-edge rejection runs BEFORE the delete_edge_types scope
            // check (spec §3.5: "existing derived-edge rejection precedes
            // delete_edge_types check").
            BatchOp::DeleteEdge {
                edge_type,
                src_key,
                dst_key,
            } => {
                // Check provenance ownership BEFORE scope (spec §3.5 ordering).
                if let (Some(src_id), Some(dst_id), Some(et_sym)) = (
                    self.ids.get(src_key.as_str()),
                    self.ids.get(dst_key.as_str()),
                    self.syms.get(edge_type.as_str()),
                ) {
                    if self.engine.is_owned(et_sym, src_id, dst_id) {
                        return Err(GraphError::RuleOwned {
                            detail: format!(
                                "edge {edge_type} {src_key}→{dst_key} is rule-owned; \
                                 delete or change the owning rule"
                            ),
                        });
                    }
                    // Also check would_derive via MutPreview (empty overlay, pre-batch).
                    let preview = MutPreview::new(self);
                    if preview.would_derive(edge_type, src_key, dst_key) {
                        return Err(GraphError::RuleOwned {
                            detail: format!(
                                "edge {edge_type} {src_key}→{dst_key} is rule-owned; \
                                 delete or change the owning rule, or a live rule would \
                                 re-derive it"
                            ),
                        });
                    }
                }
                // Scope check (AFTER derived-edge check, BEFORE endpoint visibility).
                if !authz.scope.delete_edge_types.contains(edge_type) {
                    return Err(GraphError::RoleWriteDenied {
                        reason: format!(
                            "role-bound token: edge type '{}' not in write scope (delete_edge_types)",
                            edge_type
                        ),
                    });
                }
                // Both endpoints must be visible.
                for ep_key in [src_key.as_str(), dst_key.as_str()] {
                    match self.ids.get(ep_key) {
                        None => {
                            return Err(GraphError::RoleWriteDenied {
                                reason: "role-bound token: edge endpoint not visible".into(),
                            });
                        }
                        Some(id) if !authz.mask.contains_id(id) => {
                            return Err(GraphError::RoleWriteDenied {
                                reason: "role-bound token: edge endpoint not visible".into(),
                            });
                        }
                        _ => {}
                    }
                }
            }

            // ── EDGE-CREATE: InsertEdge ──────────────────────────────────────
            //
            // Scope check BEFORE endpoint lookup (preserves timing symmetry).
            BatchOp::InsertEdge {
                edge_type,
                src_key,
                dst_key,
            } => {
                if !authz.scope.create_edge_types.contains(edge_type) {
                    return Err(GraphError::RoleWriteDenied {
                        reason: format!(
                            "role-bound token: edge type '{}' not in write scope (create_edge_types)",
                            edge_type
                        ),
                    });
                }
                // Both endpoints must be visible. A node created by an earlier
                // InsertNode in the same batch (tracked in batch_created) counts
                // as visible if its label passed the create-class gate.
                for ep_key in [src_key.as_str(), dst_key.as_str()] {
                    if batch_created.contains_key(ep_key) {
                        // Created earlier this batch — already scope-checked.
                        continue;
                    }
                    match self.ids.get(ep_key) {
                        None => {
                            return Err(GraphError::RoleWriteDenied {
                                reason: "role-bound token: edge endpoint not visible".into(),
                            });
                        }
                        Some(id) if !authz.mask.contains_id(id) => {
                            return Err(GraphError::RoleWriteDenied {
                                reason: "role-bound token: edge endpoint not visible".into(),
                            });
                        }
                        _ => {}
                    }
                }
            }

            // ── EDGE-CREATE: InsertEdgeUpsert ────────────────────────────────
            //
            // Scope check first; then endpoint visibility using same-batch
            // placeholder awareness (spec: "a placeholder endpoint the SAME
            // batch creates counts as visible if its label passed the
            // create-class gate").
            BatchOp::InsertEdgeUpsert {
                edge_type,
                src_key,
                dst_key,
                placeholder_label,
            } => {
                if !authz.scope.create_edge_types.contains(edge_type) {
                    return Err(GraphError::RoleWriteDenied {
                        reason: format!(
                            "role-bound token: edge type '{}' not in write scope (create_edge_types)",
                            edge_type
                        ),
                    });
                }
                // Check placeholder label against create_labels (create-class gate).
                // This ensures the auto-created endpoints are scope-allowed.
                for ep_key in [src_key.as_str(), dst_key.as_str()] {
                    if !upsert_ep_visible(ep_key, placeholder_label) {
                        return Err(GraphError::RoleWriteDenied {
                            reason: "role-bound token: edge endpoint not visible".into(),
                        });
                    }
                }
                // A placeholder is created with no props, so it lands in the
                // default namespace. A role that cannot read `default` must not
                // create one there, for the same reason it may not create a node
                // there outright.
                //
                // The refusal is byte-identical to the hidden-endpoint one above,
                // and deliberately so: this arm fires only for an endpoint that
                // does **not** exist, and the one above only for an endpoint that
                // does. Two different strings would make the pair an existence
                // oracle — ask for an upsert and read off whether the key is
                // taken. Hidden ≡ absent is the rule everywhere else in this
                // table and it holds here too.
                if let Some(def) = self.role_def_for(&authz.role) {
                    if !def.sees_namespace(NS_DEFAULT) {
                        for ep_key in [src_key.as_str(), dst_key.as_str()] {
                            if self.ids.get(ep_key).is_none() && !batch_created.contains_key(ep_key)
                            {
                                return Err(GraphError::RoleWriteDenied {
                                    reason: "role-bound token: edge endpoint not visible".into(),
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Write `roles` to `roles.json` atomically and update the in-memory list.
    ///
    /// Called by `apply_schema` when roles change. Never called on unchanged
    /// re-apply — this preserves byte-identical idempotency.
    pub(crate) fn commit_roles(&mut self, roles: Vec<RoleDef>) -> Result<()> {
        let file = RolesFile::new_versioned(roles.clone());
        let bytes = serde_json::to_vec(&file).map_err(|e| GraphError::Corrupt {
            detail: format!("roles serialization: {e}"),
        })?;
        self.fs
            .write_atomic(FileId::Roles, &bytes)
            .map_err(GraphError::Io)?;
        self.roles = Some(roles);
        // Rewriting the sidecar is not a commit, so `commit_seq` does not move
        // and a memoised mask would still match its version. Install a fresh
        // cache instead of clearing the shared one: a reader snapshot frozen
        // against the old definitions keeps the old `Arc` to itself and can
        // never publish an answer this handle would read back.
        self.role_masks = Arc::new(crate::mask::RoleMaskCache::new());
        // Refresh the MVCC frozen overlay so that reader() immediately sees the
        // updated role definitions without waiting for the next K-commit fold.
        self.fold_now();
        Ok(())
    }

    /// Forget the equality indexes that cover a property a view owns.
    ///
    /// A view writes its property without passing the property index, so such
    /// an index answers from stale postings while the handle lives (ledger row
    /// 123). Declaring one is refused now (`enable_index`, `create_view`), but
    /// a store written before the refusal can hold both, and must still open
    /// and answer correctly: the pair is dropped in memory, so a query on it
    /// scans, and the next snapshot no longer re-emits the declaration. Runs
    /// once, after replay, before the postings are built.
    fn drop_indexes_over_view_properties(&mut self) {
        let owned: Vec<(String, String)> = self
            .view_store
            .views()
            .filter(|v| self.prop_index.is_equality_declared(&v.label, &v.view_prop))
            .map(|v| (v.label.clone(), v.view_prop.clone()))
            .collect();
        for (label, field) in owned {
            Arc::make_mut(&mut self.prop_index).disable(&label, &field);
            self.schema_stale = true;
            // Once per pair per open: the only signal that a query on it now
            // scans, where the declaration said it would not.
            eprintln!(
                "[mushroomdb] dropped the equality index on ({label}, {field}): a view owns \
                 property '{field}' on label '{label}', so the index would answer from stale \
                 values. Queries on it are answered by scan; the next snapshot no longer \
                 carries the declaration."
            );
            self.dropped_view_indexes.push((label, field));
        }
    }

    /// The `(label, field)` equality indexes the open of this handle dropped
    /// because a view owns the property (ledger row 123), in view order. Empty
    /// unless the store was written before `enable_index` and `create_view`
    /// refused the combination.
    pub fn dropped_view_indexes(&self) -> &[(String, String)] {
        &self.dropped_view_indexes
    }

    fn view(&self) -> GraphView<'_> {
        GraphView {
            ids: &self.ids,
            syms: &self.syms,
            labels: &self.labels,
            props: self.props_view(),
            topo: self.topo_view(),
            edge_props: self.edge_props_view(),
            mask: None,
            prop_index: Some(&self.prop_index),
            layer: None,
        }
    }

    fn view_masked<'a>(&'a self, mask: &'a crate::mask::NodeMask) -> GraphView<'a> {
        GraphView {
            ids: &self.ids,
            syms: &self.syms,
            labels: &self.labels,
            props: self.props_view(),
            topo: self.topo_view(),
            edge_props: self.edge_props_view(),
            mask: Some(&mask.visible),
            prop_index: Some(&self.prop_index),
            layer: None,
        }
    }

    /// Execute a read-only Cypher query with a node visibility mask.
    ///
    /// Only nodes whose key is in `mask` are accessible: label scans, key
    /// lookups, and neighbor expansions all respect the mask. Edges where
    /// either endpoint is hidden are silently dropped.
    ///
    /// Returns `Err` with a "masked queries are read-only" message when
    /// `cypher` is a write statement (CREATE / MERGE / MATCH…SET / DELETE).
    pub fn query_masked(
        &self,
        cypher: &str,
        params: &std::collections::BTreeMap<String, Value>,
        mask: &crate::mask::NodeMask,
    ) -> Result<ResultSet> {
        // Reject write statements up front.
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        if is_write_tokens(&tokens) {
            return Err(GraphError::MaskedReadOnly);
        }
        // `SHOW …` lists only the labels the mask lets the caller see.
        if let Some(stmt) = ddl_statement(&tokens)? {
            return Ok(self
                .schema_listing()
                .show_visible(&stmt, &self.view(), &mask.visible));
        }
        // `CALL` sees only what the mask lets through.
        if let Some(call) = call_statement(&tokens)? {
            return run_call(
                &call,
                &self.view_masked(mask),
                &self.schema_listing(),
                params,
            );
        }
        let union = parse_read(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        // Each UNION part executes against the same masked view, so the mask
        // applies uniformly across the chain.
        execute_union(&self.view_masked(mask), &union, &Params(params)).map_err(|e| {
            GraphError::QueryError {
                detail: format!("execute: {e}"),
            }
        })
    }

    pub fn node_ref(&self, key: &str) -> Option<NodeRef<'_, F>> {
        let id = self.ids.get(key)?;
        Some(NodeRef { db: self, id })
    }

    /// BFS neighborhood expansion restricted to visible nodes in `mask`.
    ///
    /// Hidden nodes are never used as traversal intermediaries in either
    /// [`MaskMode::Omit`] or [`MaskMode::Stub`] — a visible node reachable
    /// only through a hidden node will not appear in results.
    ///
    /// In [`MaskMode::Stub`] mode, hidden nodes that are direct neighbours of
    /// a visited visible node are appended to the result as stub rows
    /// (`label` column is `null`, same key+depth columns as visible rows).
    /// They are NOT added to the BFS frontier.
    ///
    /// Returns `None` when `key` does not exist (caller should 404).
    ///
    /// **SECURITY**: role-token callers always pass an Omit-mode mask, so
    /// stub rows are never produced on the role path.
    pub fn neighborhood_masked(
        &self,
        key: &str,
        depth: u32,
        edge_types: Option<&[&str]>,
        dir: Dir,
        mask: &crate::mask::NodeMask,
    ) -> Option<ResultSet> {
        let start_id = self.ids.get(key)?;
        let view = self.view_masked(mask);
        let resolved: Option<Vec<u32>> = edge_types.map(|names| {
            names
                .iter()
                .filter_map(|name| view.syms.get(name))
                .collect()
        });
        let nb = neighborhood(&view, start_id, depth, resolved.as_deref(), dir);
        let mut rs = ResultSet::new(vec!["key".into(), "label".into(), "depth".into()]);
        // Collect visible BFS results (start_id at depth 0, BFS nodes after).
        let mut visited: Vec<(u32, u32)> = Vec::with_capacity(nb.nodes.len() + 1);
        visited.push((start_id, 0));
        for (nid, d) in &nb.nodes {
            let k = view.key_of(*nid);
            let label = view
                .label_of(*nid)
                .expect("real nodes always have a label; u32::MAX sentinel cannot occur");
            rs.push_row(vec![
                Some(Value::Str(k.to_string())),
                Some(Value::Str(label.to_string())),
                Some(Value::Int(*d as i64)),
            ]);
            visited.push((*nid, *d));
        }
        // Stub mode: add hidden direct neighbours of each visited node as stubs.
        // Hidden nodes are edge-endpoints only — they are not added to the BFS
        // frontier, so the BFS never expands through them.
        if mask.mode() == crate::mask::MaskMode::Stub {
            let raw_view = self.view();
            let mut seen: std::collections::HashSet<u32> =
                visited.iter().map(|(id, _)| *id).collect();
            for (node_id, node_depth) in &visited {
                if *node_depth >= depth {
                    continue;
                }
                for e in expand(&raw_view, *node_id, resolved.as_deref(), dir) {
                    let nbr = if e.src == *node_id { e.dst } else { e.src };
                    if !mask.contains_id(nbr) && seen.insert(nbr) {
                        if let Some(k) = self.ids.key_of(nbr) {
                            rs.push_row(vec![
                                Some(Value::Str(k.to_string())),
                                None,
                                Some(Value::Int((*node_depth + 1) as i64)),
                            ]);
                        }
                    }
                }
            }
        }
        Some(rs)
    }

    /// [`neighborhood_masked`](Self::neighborhood_masked) with the **subject
    /// check** a scoped caller needs: a start key the mask hides answers exactly
    /// as an absent one does.
    ///
    /// `neighborhood_masked` expands from any existing key, hidden or not,
    /// because a full-token caller supplying a client mask already knows which
    /// keys exist. A scoped caller does not, so telling it apart a hidden key
    /// from an absent one would be an existence oracle.
    ///
    /// Expansion itself is unchanged: hidden nodes are neither returned nor used
    /// as traversal intermediaries, so a visible node reachable only through a
    /// hidden one stays out of the result.
    ///
    /// Hidden or unknown `key` → [`GraphError::KeyNotFound`].
    pub fn neighborhood_scoped(
        &self,
        key: &str,
        depth: u32,
        edge_types: Option<&[&str]>,
        dir: Dir,
        mask: &crate::mask::NodeMask,
    ) -> Result<ResultSet> {
        if !mask.contains_node(self, key) {
            return Err(GraphError::KeyNotFound { key: key.into() });
        }
        self.neighborhood_masked(key, depth, edge_types, dir, mask)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })
    }

    /// Live node's key, label, and columnar props. Unknown or tombstoned → `None`.
    pub fn node_info(&self, key: &str) -> Option<NodeInfo> {
        let n = self.node_ref(key)?;
        Some(NodeInfo {
            key: n.key().to_string(),
            label: n.label().to_string(),
            props: n.props(),
        })
    }

    /// Look up a node with mask awareness.
    ///
    /// | Key state         | Omit mode       | Stub mode              |
    /// |-------------------|-----------------|------------------------|
    /// | does not exist    | `None` (→ 404)  | `None` (→ 404)         |
    /// | exists, visible   | `Some(Visible)` | `Some(Visible)`        |
    /// | exists, hidden    | `None` (→ 404)  | `Some(Restricted)`     |
    ///
    /// **SECURITY**: only call from client-mask (full-token) paths.
    /// Role-token paths must use [`node_info`] after an explicit visibility check.
    pub fn node_info_masked(
        &self,
        key: &str,
        mask: &crate::mask::NodeMask,
    ) -> Option<MaskedNodeResult> {
        let id = self.ids.get(key)?;
        if mask.contains_id(id) {
            Some(MaskedNodeResult::Visible(self.node_info(key)?))
        } else {
            match mask.mode() {
                crate::mask::MaskMode::Stub => Some(MaskedNodeResult::Restricted),
                crate::mask::MaskMode::Omit => None,
            }
        }
    }

    /// Get edges for `key` with mask-aware hidden-endpoint handling.
    ///
    /// - Omit mode: edges to hidden endpoints are excluded (same as role-path filtering).
    /// - Stub mode: edges to hidden endpoints are included; `src_restricted`/`dst_restricted`
    ///   is `true` for each hidden endpoint.
    ///
    /// Unknown key → [`GraphError::KeyNotFound`].
    ///
    /// **SECURITY**: only call from client-mask (full-token) paths.
    pub fn node_edges_masked(
        &self,
        key: &str,
        mask: &crate::mask::NodeMask,
    ) -> Result<Vec<MaskedEdge>> {
        self.ensure_v8_base_sections_loaded();
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
        let derived: BTreeSet<(u32, u32, u32)> = self
            .engine
            .provenance_touching(id)
            .map(|(_rule, etype, src, dst)| (etype, src, dst))
            .collect();
        let mut edges = Vec::new();
        let tv = self.topo_view();
        for etype in tv.etypes() {
            // etype comes from the archived CSR (access_unchecked, no eager CRC).
            // A bit-flip in the large TOPOLOGY section can produce an etype id
            // that is not in the interner.  Return Corrupt rather than panic.
            let edge_type = self
                .syms
                .resolve(etype)
                .ok_or_else(|| GraphError::Corrupt {
                    detail: format!("v8: topology etype {etype} not in interner"),
                })?
                .to_string();
            for dir in [Direction::Out, Direction::In] {
                for &nbr in tv.neighbors(etype, dir, id).as_ref() {
                    let nbr_restricted = !mask.contains_id(nbr);
                    if nbr_restricted && mask.mode() == crate::mask::MaskMode::Omit {
                        continue;
                    }
                    let nbr_key = self
                        .ids
                        .key_of(nbr)
                        .ok_or_else(|| GraphError::Corrupt {
                            detail: format!("topology id {nbr} has no key"),
                        })?
                        .to_string();
                    let (src_id, dst_id, src_key, dst_key, src_restricted, dst_restricted) =
                        match dir {
                            Direction::Out => {
                                (id, nbr, key.to_string(), nbr_key, false, nbr_restricted)
                            }
                            Direction::In => {
                                (nbr, id, nbr_key, key.to_string(), nbr_restricted, false)
                            }
                        };
                    edges.push(MaskedEdge {
                        edge_type: edge_type.clone(),
                        src_key,
                        src_restricted,
                        dst_key,
                        dst_restricted,
                        derived: derived.contains(&(etype, src_id, dst_id)),
                    });
                }
            }
        }
        edges.sort_by(|a, b| {
            a.edge_type
                .cmp(&b.edge_type)
                .then(a.src_key.cmp(&b.src_key))
                .then(a.dst_key.cmp(&b.dst_key))
        });
        edges.dedup_by(|a, b| {
            a.edge_type == b.edge_type && a.src_key == b.src_key && a.dst_key == b.dst_key
        });
        Ok(edges)
    }

    /// [`node_edges_masked`](Self::node_edges_masked) with the **subject check**
    /// a scoped caller needs, and a plain [`EdgeInfo`] list.
    ///
    /// `node_edges_masked` raises [`GraphError::KeyNotFound`] only when `key` is
    /// unknown; a key that exists but is hidden still yields its (filtered) edge
    /// list, which is correct for a full-token client mask and an existence
    /// oracle for a scoped one. Here a hidden subject answers exactly as an
    /// absent one does.
    ///
    /// Every edge naming a hidden endpoint is dropped, whatever the mask's
    /// [`MaskMode`](crate::mask::MaskMode): a scoped caller never sees a
    /// restricted stub, so there is nothing for it to render.
    ///
    /// Hidden or unknown `key` → [`GraphError::KeyNotFound`].
    pub fn node_edges_scoped(
        &self,
        key: &str,
        mask: &crate::mask::NodeMask,
    ) -> Result<Vec<EdgeInfo>> {
        if !mask.contains_node(self, key) {
            return Err(GraphError::KeyNotFound { key: key.into() });
        }
        Ok(self
            .node_edges_masked(key, mask)?
            .into_iter()
            .filter(|e| !e.src_restricted && !e.dst_restricted)
            .map(|e| EdgeInfo {
                edge_type: e.edge_type,
                src_key: e.src_key,
                dst_key: e.dst_key,
                derived: e.derived,
            })
            .collect())
    }

    /// Every directed edge incident on `key`, both directions, every etype.
    ///
    /// Walk is `topology.etypes()` × `{Out, In}` × `neighbors()`. `derived` is
    /// membership in [`RuleEngine::provenance_touching`] (O(degree) via the
    /// Plan-8 `by_node` index). Sorted by `(edge_type, src_key, dst_key)`.
    /// Unknown key → [`GraphError::KeyNotFound`].
    pub fn node_edges(&self, key: &str) -> Result<Vec<EdgeInfo>> {
        self.ensure_v8_base_sections_loaded();
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
        let derived: BTreeSet<(u32, u32, u32)> = self
            .engine
            .provenance_touching(id)
            .map(|(_rule, etype, src, dst)| (etype, src, dst))
            .collect();
        let mut edges = Vec::new();
        let tv = self.topo_view();
        for etype in tv.etypes() {
            // Same guard as node_edges_masked: etype from unchecked-CRC CSR.
            let edge_type = self
                .syms
                .resolve(etype)
                .ok_or_else(|| GraphError::Corrupt {
                    detail: format!("v8: topology etype {etype} not in interner"),
                })?
                .to_string();
            for dir in [Direction::Out, Direction::In] {
                for &nbr in tv.neighbors(etype, dir, id).as_ref() {
                    let (src, dst, src_key, dst_key) = match dir {
                        Direction::Out => (
                            id,
                            nbr,
                            key.to_string(),
                            self.ids
                                .key_of(nbr)
                                .ok_or_else(|| GraphError::Corrupt {
                                    detail: format!("topology id {nbr} has no key"),
                                })?
                                .to_string(),
                        ),
                        Direction::In => (
                            nbr,
                            id,
                            self.ids
                                .key_of(nbr)
                                .ok_or_else(|| GraphError::Corrupt {
                                    detail: format!("topology id {nbr} has no key"),
                                })?
                                .to_string(),
                            key.to_string(),
                        ),
                    };
                    edges.push(EdgeInfo {
                        edge_type: edge_type.clone(),
                        src_key,
                        dst_key,
                        derived: derived.contains(&(etype, src, dst)),
                    });
                }
            }
        }
        edges.sort_by(|a, b| {
            a.edge_type
                .cmp(&b.edge_type)
                .then(a.src_key.cmp(&b.src_key))
                .then(a.dst_key.cmp(&b.dst_key))
        });
        // Self-loops appear in both Out and In; sort makes the pair adjacent
        // (sort key matches PartialEq for this case) so one pass drops the dup.
        edges.dedup();
        Ok(edges)
    }

    // ── Backup ────────────────────────────────────────────────────────────────

    /// Copy this store to `dest` as a consistent, verified snapshot.
    ///
    /// Copies every durable file in the database directory — `snapshot.bin`,
    /// `wal.bin`, all `wal.<N>.archive` files, `wal.floor`, `wal.genesis`, and
    /// `roles.json` — into a freshly created `dest` directory using OS-level
    /// `copy` calls (no large in-process buffers).
    ///
    /// # Consistency guarantee
    ///
    /// The guarantee is **process-local**: the caller holds `&self`, which
    /// prevents any concurrent writer in the **same process** from modifying
    /// the files during the copy.  Running `mushroomdb backup` against a
    /// directory that is **concurrently being written by another process** (e.g.
    /// `mushroomdb serve`) is **unsafe** — the copy can be torn.  The post-copy
    /// `verified: true` result reduces but does not eliminate the risk of a
    /// silent corrupt backup (CRC catches many bit-flips; it cannot catch a
    /// consistent mid-write snapshot).
    ///
    /// **The safe path for a live-served store is `POST /backup` on the HTTP
    /// server.** That handler acquires the read lock on the shared database
    /// before calling this method, which is the correct cross-process
    /// synchronisation point because the server is the single process writing
    /// the files.
    ///
    /// After copying, opens the destination read-only and runs the CRC section
    /// verifier (`verify_snapshot`) to confirm byte-for-byte integrity.
    /// `BackupReport::verified` reflects whether both checks passed.
    ///
    /// Returns `Err` when `self` is not backed by a `RealFs` (e.g. `SimFs`).
    pub fn backup_to(&self, dest: &std::path::Path) -> Result<BackupReport> {
        // Derive source directory from snapshot_path (RealFs only).
        let src_dir = match self.fs.snapshot_path() {
            Some(p) => p.parent().map(|d| d.to_path_buf()).ok_or_else(|| {
                GraphError::Io(std::io::Error::other("snapshot has no parent dir"))
            })?,
            None => {
                return Err(GraphError::Io(std::io::Error::other(
                    "backup_to requires a real filesystem (RealFs)",
                )))
            }
        };

        std::fs::create_dir_all(dest)?;

        let mut files: Vec<String> = Vec::new();
        let mut bytes: u64 = 0;

        // Helper: copy src_dir/name → dest/name if the file exists.
        let mut try_copy = |name: &str| -> std::io::Result<()> {
            let src_path = src_dir.join(name);
            if src_path.exists() {
                let n = std::fs::copy(&src_path, dest.join(name))?;
                bytes += n;
                files.push(name.to_string());
            }
            Ok(())
        };

        try_copy("snapshot.bin")?;
        try_copy("snapshot.bin.bak")?;
        try_copy("wal.bin")?;
        try_copy("wal.floor")?;
        try_copy("wal.genesis")?;
        // A V11 store's genesis marker (see `Fs::use_v11_genesis_marker`).
        try_copy("wal.genesis.v11")?;
        try_copy("roles.json")?;
        // The map from commit to wall-clock time. Without it the copy cannot
        // resolve a date to a commit, and answers `NoRecordedTime` for history
        // it holds in full (ledger row 86).
        try_copy("commit_times.bin")?;
        // Empty unless the store's last WAL-replacing snapshot was
        // interrupted. Then, without it, the copy is that snapshot beside the
        // WAL it was made from, with nothing to say so.
        try_copy("snapshot.intent")?;

        // Copy WAL archives.
        let archives = self.fs.list_archives()?;
        for n in &archives {
            let name = format!("wal.{n}.archive");
            let n_bytes = std::fs::copy(src_dir.join(&name), dest.join(&name))?;
            bytes += n_bytes;
            files.push(name);
        }

        files.sort();

        // Post-copy verification: open dest and run CRC checks.
        let snap_in_dest = dest.join("snapshot.bin").exists();
        let crc_ok = if snap_in_dest {
            crate::verify_snapshot(dest)
                .map(|results| results.iter().all(|(_, _, _, r)| r.is_ok()))
                .unwrap_or(false)
        } else {
            true // WAL-only store: nothing to CRC-check in snapshot
        };
        let opens_ok = GraphDb::<core_storage::fs::RealFs>::open(dest).is_ok();
        let verified = crc_ok && opens_ok;

        Ok(BackupReport {
            files,
            bytes,
            verified,
        })
    }

    // ── Export helpers ────────────────────────────────────────────────────────

    /// All live nodes, sorted by key (deterministic).
    ///
    /// Reads base + WAL overlay. Tombstoned nodes are excluded.
    pub fn all_nodes_for_export(&self) -> Vec<NodeInfo> {
        self.ensure_v8_base_sections_loaded();
        let pv = self.props_view();
        let mut nodes = Vec::new();
        for id in 0..self.ids.len() as u32 {
            let Some(key) = self.ids.key_of(id) else {
                continue;
            };
            let Some(&sym) = self.labels.get(id as usize) else {
                continue;
            };
            if sym == u32::MAX {
                continue; // tombstoned
            }
            let Some(label) = self.syms.resolve(sym) else {
                continue;
            };
            let mut props = BTreeMap::new();
            for field in pv.field_names() {
                if let Some(vr) = pv.get(id, &field) {
                    props.insert(field, vr.into_value());
                }
            }
            nodes.push(NodeInfo {
                key: key.to_string(),
                label: label.to_string(),
                props,
            });
        }
        nodes.sort_by(|a, b| a.key.cmp(&b.key));
        nodes
    }

    /// All directed edges, sorted by `(edge_type, src, dst)`. Each edge appears once.
    ///
    /// Derived edges carry `derived: true` and the creating rule's name in `rule`.
    /// Manual edges carry `derived: false` and `rule: None`.
    /// `weight` is the creating rule's `weight_prop` value read off the edge
    /// (numeric only), mirroring the convention used by [`GraphDb::explain`]
    /// and [`GraphDb::weighted_edges`]. Deterministic across runs on the same
    /// store state.
    pub fn all_edges_for_export(&self) -> Vec<ExportEdge> {
        self.ensure_v8_base_sections_loaded();

        // Build (etype_sym, src_id, dst_id) → rule_name for O(1) derivation lookup.
        let mut prov: HashMap<(u32, u32, u32), String> = HashMap::new();
        for (rule_name, triples) in self.engine.provenance() {
            for &(etype, src, dst) in triples {
                prov.insert((etype, src, dst), rule_name.clone());
            }
        }

        // rule_name → weight_prop, for O(1) lookup per derived edge.
        let weight_props: HashMap<&str, Option<&str>> = self
            .engine
            .rules()
            .map(|r| (r.name.as_str(), r.weight_prop.as_deref()))
            .collect();

        let tv = self.topo_view();
        let ep = self.edge_props_view();
        let mut edges = Vec::new();

        for id in 0..self.ids.len() as u32 {
            let Some(key) = self.ids.key_of(id) else {
                continue;
            };
            let Some(&lsym) = self.labels.get(id as usize) else {
                continue;
            };
            if lsym == u32::MAX {
                continue; // tombstoned
            }

            for etype_sym in tv.etypes() {
                // etype from archived CSR (access_unchecked, no eager CRC).
                // Skip edges whose etype is not in the interner; this can only
                // occur with a corrupt large TOPOLOGY section (bit-flip on an
                // etype field in the archived data).  The function returns Vec,
                // not Result, so we continue rather than propagate.
                let Some(edge_type) = self.syms.resolve(etype_sym) else {
                    continue;
                };
                let edge_type = edge_type.to_string();
                for &nbr in tv.neighbors(etype_sym, Direction::Out, id).as_ref() {
                    let Some(dst_key) = self.ids.key_of(nbr) else {
                        continue; // skip corrupt entries
                    };
                    let prov_key = (etype_sym, id, nbr);
                    let rule = prov.get(&prov_key).cloned();
                    let derived = rule.is_some();
                    let weight = rule
                        .as_deref()
                        .and_then(|rn| weight_props.get(rn).copied().flatten())
                        .and_then(|prop| match ep.get(etype_sym, id, nbr, prop) {
                            Some(Value::Float(f)) => Some(f),
                            Some(Value::Int(i)) => Some(i as f64),
                            _ => None,
                        });
                    edges.push(ExportEdge {
                        edge_type: edge_type.clone(),
                        src: key.to_string(),
                        dst: dst_key.to_string(),
                        derived,
                        rule,
                        weight,
                    });
                }
            }
        }

        edges.sort_by(|a, b| {
            a.edge_type
                .cmp(&b.edge_type)
                .then(a.src.cmp(&b.src))
                .then(a.dst.cmp(&b.dst))
        });
        edges
    }

    /// [`all_nodes_for_export`](Self::all_nodes_for_export), one node at a time.
    ///
    /// Calls `each` with every live node in key order, and stops at its first
    /// `Err`. Only the sorted list of node ids (four bytes a node) is held; a
    /// node's property map is built when it is handed over and dropped when
    /// `each` returns, so an export's memory does not grow with the properties
    /// of the store.
    pub fn for_each_node_for_export<E>(
        &self,
        mut each: impl FnMut(NodeInfo) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        self.ensure_v8_base_sections_loaded();
        let pv = self.props_view();
        let fields = pv.field_names();
        let mut ids: Vec<u32> = (0..self.ids.len() as u32)
            .filter(|&id| {
                self.ids.key_of(id).is_some()
                    && self
                        .labels
                        .get(id as usize)
                        .is_some_and(|&sym| sym != u32::MAX && self.syms.resolve(sym).is_some())
            })
            .collect();
        ids.sort_by(|&a, &b| self.ids.key_of(a).cmp(&self.ids.key_of(b)));
        for id in ids {
            let (Some(key), Some(&sym)) = (self.ids.key_of(id), self.labels.get(id as usize))
            else {
                continue;
            };
            let Some(label) = self.syms.resolve(sym) else {
                continue;
            };
            let mut props = BTreeMap::new();
            for field in &fields {
                if let Some(vr) = pv.get(id, field) {
                    props.insert(field.clone(), vr.into_value());
                }
            }
            each(NodeInfo {
                key: key.to_string(),
                label: label.to_string(),
                props,
            })?;
        }
        Ok(())
    }

    /// [`all_edges_for_export`](Self::all_edges_for_export), one edge at a
    /// time, in the same `(edge_type, src, dst)` order.
    ///
    /// Held at once: the sorted node ids, the edge types, and the derived-edge
    /// provenance as borrowed rule names (twelve bytes and a pointer per derived
    /// edge, none for a store with no rules). One source node's out-edges at a
    /// time are materialised.
    pub fn for_each_edge_for_export<E>(
        &self,
        mut each: impl FnMut(ExportEdge) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        self.ensure_v8_base_sections_loaded();
        let mut prov: HashMap<(u32, u32, u32), &str> = HashMap::new();
        for (rule_name, triples) in self.engine.provenance() {
            for &triple in triples {
                prov.insert(triple, rule_name.as_str());
            }
        }
        let weight_props: HashMap<&str, Option<&str>> = self
            .engine
            .rules()
            .map(|r| (r.name.as_str(), r.weight_prop.as_deref()))
            .collect();
        let tv = self.topo_view();
        let ep = self.edge_props_view();

        let mut etypes: Vec<(&str, u32)> = tv
            .etypes()
            .filter_map(|sym| self.syms.resolve(sym).map(|name| (name, sym)))
            .collect();
        etypes.sort();
        let mut srcs: Vec<u32> = (0..self.ids.len() as u32)
            .filter(|&id| {
                self.ids.key_of(id).is_some()
                    && self
                        .labels
                        .get(id as usize)
                        .is_some_and(|&sym| sym != u32::MAX)
            })
            .collect();
        srcs.sort_by(|&a, &b| self.ids.key_of(a).cmp(&self.ids.key_of(b)));

        for (edge_type, etype_sym) in etypes {
            for &id in &srcs {
                let Some(src) = self.ids.key_of(id) else {
                    continue;
                };
                let mut out: Vec<(&str, u32)> = tv
                    .neighbors(etype_sym, Direction::Out, id)
                    .as_ref()
                    .iter()
                    .filter_map(|&nbr| self.ids.key_of(nbr).map(|k| (k, nbr)))
                    .collect();
                out.sort();
                for (dst, nbr) in out {
                    let rule = prov.get(&(etype_sym, id, nbr)).copied();
                    let weight = rule
                        .and_then(|rn| weight_props.get(rn).copied().flatten())
                        .and_then(|prop| match ep.get(etype_sym, id, nbr, prop) {
                            Some(Value::Float(f)) => Some(f),
                            Some(Value::Int(i)) => Some(i as f64),
                            _ => None,
                        });
                    each(ExportEdge {
                        edge_type: edge_type.to_string(),
                        src: src.to_string(),
                        dst: dst.to_string(),
                        derived: rule.is_some(),
                        rule: rule.map(str::to_string),
                        weight,
                    })?;
                }
            }
        }
        Ok(())
    }

    /// What each edge type *is*, without building one record per edge.
    ///
    /// [`all_edges_for_export`](Self::all_edges_for_export) answers the same
    /// question by materialising every edge — three `String`s apiece, a
    /// provenance `HashMap` over every derived edge, and a final sort. That is
    /// the right shape for an export, and the wrong one for a summary: on a
    /// store with 1.3 M derived edges it allocates hundreds of megabytes to
    /// produce nine lines. This walks the topology instead, summing neighbour
    /// slice lengths and collecting *label symbols* rather than label strings,
    /// so the per-edge cost is an integer add and a set insert on a set with
    /// as many members as the store has labels.
    ///
    /// The rule names come off the rule *definitions*, which each declare the
    /// `edge_type` they derive, so naming them costs one pass over the rules
    /// rather than one provenance lookup per edge. That is also why `rules`
    /// is a list: two rules may derive the same type — the association store
    /// derives `INDUSTRY_ALIGNMENT` from both a talent→company and a
    /// talent→job rule — and naming only one of them would be a half-truth.
    /// A type with no rules is one written by hand.
    ///
    /// `sample` is the first edge of the type in the store's own id order,
    /// which is insertion order: deterministic for a given store, and not the
    /// same as key order, which cannot be had without resolving a key per
    /// edge. Sorted by `edge_type`.
    pub fn edge_type_census(&self) -> Vec<EdgeTypeCensus> {
        self.ensure_v8_base_sections_loaded();

        let mut rules_by_type: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for r in self.engine.rules() {
            rules_by_type
                .entry(r.edge_type.as_str())
                .or_default()
                .insert(r.name.as_str());
        }

        let tv = self.topo_view();
        let node_count = self.ids.len() as u32;
        let mut out = Vec::new();
        for etype_sym in tv.etypes() {
            // An etype the interner cannot resolve means a corrupt TOPOLOGY
            // section; skip it rather than name it, as `all_edges_for_export`
            // does for the same reason.
            let Some(edge_type) = self.syms.resolve(etype_sym) else {
                continue;
            };
            let mut edges: u64 = 0;
            let mut src_syms: BTreeSet<u32> = BTreeSet::new();
            let mut dst_syms: BTreeSet<u32> = BTreeSet::new();
            let mut sample: Option<(u32, u32)> = None;
            for id in 0..node_count {
                let Some(&lsym) = self.labels.get(id as usize) else {
                    continue;
                };
                if lsym == u32::MAX {
                    continue; // tombstoned
                }
                let nbrs = tv.neighbors(etype_sym, Direction::Out, id);
                let nbrs = nbrs.as_ref();
                if nbrs.is_empty() {
                    continue;
                }
                edges += nbrs.len() as u64;
                src_syms.insert(lsym);
                for &nbr in nbrs {
                    if let Some(&dsym) = self.labels.get(nbr as usize) {
                        if dsym != u32::MAX {
                            dst_syms.insert(dsym);
                        }
                    }
                }
                if sample.is_none() {
                    sample = Some((id, nbrs[0]));
                }
            }
            let resolve = |syms: &BTreeSet<u32>| -> Vec<String> {
                syms.iter()
                    .filter_map(|&s| self.syms.resolve(s))
                    .map(ToString::to_string)
                    .collect()
            };
            out.push(EdgeTypeCensus {
                edge_type: edge_type.to_string(),
                edges,
                src_labels: resolve(&src_syms),
                dst_labels: resolve(&dst_syms),
                rules: rules_by_type
                    .get(edge_type)
                    .map(|rs| rs.iter().map(ToString::to_string).collect())
                    .unwrap_or_default(),
                sample: sample.and_then(|(s, d)| {
                    Some((
                        self.ids.key_of(s)?.to_string(),
                        self.ids.key_of(d)?.to_string(),
                    ))
                }),
            });
        }
        out.sort_by(|a, b| a.edge_type.cmp(&b.edge_type));
        out
    }

    /// All directed edges of `edge_type`, with the raw value of `weight_prop`
    /// on each edge when given.
    ///
    /// `weight` is `Some(f)` only when `weight_prop` is set and the edge
    /// carries that property with a numeric (`Int`/`Float`) value; otherwise
    /// `None` — callers that want a default weight (e.g. `1.0` for missing
    /// props) apply it themselves, matching the convention used internally
    /// by [`GraphDb::pagerank`], [`GraphDb::connected_components`],
    /// [`GraphDb::degree_centrality`], and [`GraphDb::communities`].
    ///
    /// Sorted by `(src, dst)` for determinism. Reads the unified topology
    /// (manual + rule-derived edges).  An unknown `edge_type` returns an
    /// empty vec.
    pub fn weighted_edges(
        &self,
        edge_type: &str,
        weight_prop: Option<&str>,
    ) -> Vec<(String, String, Option<f64>)> {
        let Some(etype_sym) = self.syms.get(edge_type) else {
            return Vec::new();
        };
        let tv = self.topo_view();
        let ep = self.edge_props_view();
        let mut out = Vec::new();
        for id in 0..self.ids.len() as u32 {
            let Some(key) = self.ids.key_of(id) else {
                continue;
            };
            let Some(&sym) = self.labels.get(id as usize) else {
                continue;
            };
            if sym == u32::MAX {
                continue; // tombstoned
            }
            for &nbr in tv.neighbors(etype_sym, Direction::Out, id).as_ref() {
                let Some(dst_key) = self.ids.key_of(nbr) else {
                    continue;
                };
                let weight = weight_prop.and_then(|prop| match ep.get(etype_sym, id, nbr, prop) {
                    Some(Value::Float(f)) => Some(f),
                    Some(Value::Int(i)) => Some(i as f64),
                    _ => None,
                });
                out.push((key.to_string(), dst_key.to_string(), weight));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        out
    }

    pub fn nodes_with_label(&self, label: &str) -> Vec<NodeRef<'_, F>> {
        self.view()
            .nodes_with_label(label)
            .into_iter()
            .map(|id| NodeRef { db: self, id })
            .collect()
    }

    /// Whether any of the first `limit` live nodes under `label`, in id
    /// order, carries `field`.
    ///
    /// Bounded for a caller that asks on every request: one props view, at
    /// most `limit` property reads, no value cloned and no `NodeRef` built,
    /// and it stops at the first carrier. The label column itself has no
    /// per-label index, so on a label with fewer than `limit` nodes the walk
    /// of that column runs to its end — integer compares, no property read.
    pub(crate) fn first_nodes_carry_prop(&self, label: &str, field: &str, limit: usize) -> bool {
        let Some(sym) = self.syms.get(label) else {
            return false;
        };
        let props = self.props_view();
        self.labels
            .iter()
            .enumerate()
            .filter(|&(_, &s)| s == sym)
            .take(limit)
            .any(|(id, _)| props.get(id as u32, field).is_some())
    }

    pub fn find_nodes(&self, label: &str, filter: &Filter) -> Vec<NodeRef<'_, F>> {
        let view = self.view();
        view.nodes_with_label(label)
            .into_iter()
            .filter(|&id| {
                eval_filter(filter, &|field| {
                    view.prop(id, field).map(|vr| vr.into_value())
                })
            })
            .map(|id| NodeRef { db: self, id })
            .collect()
    }

    /// Returns `true` if any approximate (HNSW) VectorSimilar rule covers
    /// `field`.  Use as a capability probe: when `true`, `find_similar_vector`
    /// with `label = None` will use the native ANN path rather than the O(n)
    /// brute-force scan.
    pub fn has_vector_rule(&self, field: &str) -> bool {
        self.engine.hnsw_has_rule(field)
    }

    /// How many HNSW graphs this handle has built from scratch since it was
    /// opened (one per side of an approximate rule).
    ///
    /// An open that restored every graph from the snapshot reports `0`.
    /// Exposed for tests that assert the open path reuses the persisted index
    /// rather than rebuilding it; not part of the stable surface.
    #[doc(hidden)]
    pub fn hnsw_build_count(&self) -> u64 {
        self.engine.hnsw_build_count()
    }

    /// Whether the rule engine has built its candidate indexes on this handle.
    ///
    /// A store opened from a snapshot defers that full node scan until the
    /// first write or the first read that needs it. Exposed for tests that
    /// assert an open did not pay for it; not part of the stable surface.
    #[doc(hidden)]
    pub fn rule_indexes_populated(&self) -> bool {
        self.engine.indexes_populated()
    }

    /// How many rules this handle still holds a lazily-decoded HNSW graph for.
    ///
    /// Zero before the first ANN query on a clean open, and again once the
    /// live indexes own the graphs. See [`core_rules::RuleEngine::lazy_hnsw_len`].
    /// Exposed for tests that assert the lazy copies are released; not part of
    /// the stable surface.
    #[doc(hidden)]
    pub fn lazy_hnsw_len(&self) -> usize {
        self.engine.lazy_hnsw_len()
    }

    /// Find nodes whose `field` vector is most similar to `q` (cosine
    /// similarity), returning up to `k` results with similarity ≥ `min`,
    /// sorted descending.
    ///
    /// When `label` is `None` the search spans all labels (via
    /// `hnsw_search_any_dst` or a full brute-force scan); when `label` is
    /// `Some(lbl)` it restricts to nodes with that label.
    ///
    /// Uses the HNSW index when one is available (fast path); otherwise falls
    /// back to an O(n) brute-force scan.
    ///
    /// **The index supplies candidates, never scores.** Its own distances are
    /// `f32` (accurate to ~1e-6, so an exact duplicate scores 0.9999999), so
    /// every candidate is re-scored from the `f64` property vectors by
    /// [`exact_vector_similarity`] before `min`, the ordering and the reported
    /// score are decided. `k + VECTOR_RESCORE_MARGIN` candidates are fetched so
    /// the re-ordering cannot drop a true top-`k` member; see that constant for
    /// the rule. The score a caller receives is therefore the same number the
    /// brute-force path would have produced, to `f64` precision, and `min = 1.0`
    /// finds an exact duplicate.
    pub fn find_similar_vector(
        &self,
        field: &str,
        label: Option<&str>,
        q: &[f64],
        k: usize,
        min: f64,
    ) -> Vec<(String, f64)> {
        self.find_similar_vector_filtered(field, label, q, k, min, None, None, false)
            .expect("find_similar_vector_filtered is infallible without where_")
    }

    /// Like [`find_similar_vector`] but restricts results to nodes visible in
    /// `mask`. Hidden nodes never appear in results; the mask is applied
    /// **before** k-truncation so a caller still receives up to `k` visible
    /// hits.
    ///
    /// # HNSW path (widening beam)
    ///
    /// When an HNSW index covers the request, the beam starts at an over-fetch
    /// of `k × n / |visible|` (plus the rescore margin) when the mask's
    /// selectivity is known from the index length, otherwise at `k` plus that
    /// margin. If fewer than `k` visible candidates remain after the mask and
    /// `min` filter, the beam doubles — the same ×2 loop exact `VectorSimilar`
    /// rules use, capped at `ef_max()` (`EF_MAX` = 4,096). Reaching the cap,
    /// or a beam that comes back short of its own width, falls through to the
    /// exhaustive masked scan rather than returning a short result.
    ///
    /// Every surviving candidate is re-scored from the `f64` property vectors,
    /// exactly as [`find_similar_vector`] does and for the same reason.
    ///
    /// # Brute-force path
    ///
    /// When no HNSW index covers the request, or the beam cannot admit `k`
    /// hits, the function builds a masked [`GraphView`] so that `nodes_all` /
    /// `nodes_with_label` return only visible nodes, guaranteeing exact `k`
    /// results (or all visible nodes if fewer than `k` exist).
    pub fn find_similar_vector_masked(
        &self,
        field: &str,
        label: Option<&str>,
        q: &[f64],
        k: usize,
        min: f64,
        mask: &crate::mask::NodeMask,
    ) -> Vec<(String, f64)> {
        self.find_similar_vector_filtered(field, label, q, k, min, Some(mask), None, false)
            .expect("find_similar_vector_filtered is infallible without where_")
    }

    /// Exact or ANN kNN with optional key-list `mask` and property `where_`.
    ///
    /// `where_` present and failing [`PropPredicate::validate_named`] `"where"`
    /// → `QueryError`. `exact=true` or `where_=Some` skip HNSW and GEMM-brute
    /// the candidate set (`label ∩ mask ∩ holds(where)`). `mask` alone still
    /// uses HNSW when an index covers the field.
    #[allow(clippy::too_many_arguments)]
    pub fn find_similar_vector_filtered(
        &self,
        field: &str,
        label: Option<&str>,
        q: &[f64],
        k: usize,
        min: f64,
        mask: Option<&crate::mask::NodeMask>,
        where_: Option<&PropPredicate>,
        exact: bool,
    ) -> Result<Vec<(String, f64)>> {
        self.find_similar_vector_as(
            field,
            label,
            q,
            k,
            min,
            mask,
            where_,
            exact,
            ExactnessCaller::Vector,
        )
    }

    /// [`find_similar_vector_filtered`](Self::find_similar_vector_filtered)
    /// with the caller shape named, so the exactness warning can advise the
    /// signature that actually reached it. Everything else is identical.
    #[allow(clippy::too_many_arguments)]
    fn find_similar_vector_as(
        &self,
        field: &str,
        label: Option<&str>,
        q: &[f64],
        k: usize,
        min: f64,
        mask: Option<&crate::mask::NodeMask>,
        where_: Option<&PropPredicate>,
        exact: bool,
        caller: ExactnessCaller,
    ) -> Result<Vec<(String, f64)>> {
        if let Some(pred) = where_ {
            pred.validate_named("where")
                .map_err(|detail| GraphError::QueryError { detail })?;
        }

        // Ensure any HNSW blobs retained from the snapshot are deserialized
        // before the first ANN query on a clean-open (no-WAL) path.  The
        // section read has to come first: on a clean open nothing else has
        // called it, so without it `retained_hnsw_blobs` is empty,
        // `ensure_hnsw_loaded` caches an empty map in its `OnceLock`, and every
        // approximate query on the handle runs brute force — correct results,
        // silently off the index.  Both calls are idempotent and cheap once hot.
        self.ensure_v8_base_sections_loaded();
        self.engine.ensure_hnsw_loaded();
        let norm: f64 = q.iter().map(|x| x * x).sum::<f64>().sqrt();
        if norm == 0.0 {
            return Ok(vec![]);
        }
        if let Some(m) = mask {
            if k == 0 || m.is_empty() {
                return Ok(vec![]);
            }
        }
        let q_unit: Vec<f64> = q.iter().map(|x| x / norm).collect();

        // `where` implies exact: a predicate must not ride a silent ANN.
        let skip_hnsw = exact || where_.is_some();
        if !skip_hnsw {
            if let Some(mask) = mask {
                if let Some(out) =
                    self.find_similar_hnsw_masked(field, label, &q_unit, k, min, mask, caller)
                {
                    return Ok(out);
                }
            } else if let Some(out) = self.find_similar_hnsw(field, label, &q_unit, k, min) {
                return Ok(out);
            }
        }

        let view = match mask {
            Some(m) => self.view_masked(m),
            None => self.view(),
        };
        let candidate_ids = Self::vector_candidates(&view, label, where_);
        Ok(self.brute_vector_hits(&view, candidate_ids, field, &q_unit, k, min))
    }

    /// Unmasked HNSW path. `None` when no populated index covers the request.
    fn find_similar_hnsw(
        &self,
        field: &str,
        label: Option<&str>,
        q_unit: &[f64],
        k: usize,
        min: f64,
    ) -> Option<Vec<(String, f64)>> {
        // Try HNSW fast path.
        // `None` label searches across all VectorSimilar rules covering `field`
        // (merging their results); `Some(lbl)` restricts to rules whose
        // dst_label matches.  Returns `None` when no populated HNSW index
        // covers the request — the O(n) brute-force fallback handles that case.
        let over_k = k.saturating_add(VECTOR_RESCORE_MARGIN);
        let hits = match label {
            Some(lbl) => self.engine.hnsw_search_dst(field, lbl, q_unit, over_k)?,
            None => self.engine.hnsw_search_any_dst(field, q_unit, over_k)?,
        };
        // Candidates only: the index's `f32` similarity is discarded and
        // each hit is re-scored against the `f64` vectors.
        let view = self.view();
        let mut out: Vec<(String, f64)> = hits
            .into_iter()
            .filter_map(|(id, _)| {
                let sim = exact_vector_similarity(&view, id, field, q_unit)?;
                if sim < min {
                    return None;
                }
                Some((self.ids.key_of(id)?.to_string(), sim))
            })
            .collect();
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        out.truncate(k);
        Some(out)
    }

    /// Masked HNSW widening beam. `None` when no index covers the request or
    /// the beam cannot admit `k` visible hits (caller falls through to brute).
    #[allow(clippy::too_many_arguments)]
    fn find_similar_hnsw_masked(
        &self,
        field: &str,
        label: Option<&str>,
        q_unit: &[f64],
        k: usize,
        min: f64,
        mask: &crate::mask::NodeMask,
        caller: ExactnessCaller,
    ) -> Option<Vec<(String, f64)>> {
        let index_len = match label {
            Some(lbl) => self.engine.hnsw_dst_len(field, lbl, q_unit.len()),
            None => self.engine.hnsw_any_dst_len(field, q_unit.len()),
        };
        let n = index_len?;
        // The `?` above is the coverage test: past it, an index exists and this
        // masked, non-exact call is about to ride it.
        self.note_ambiguous_exactness(field, label, caller);
        // Same ceiling the exact-rule widening loop in `hnsw_candidates`
        // consults — including the `with_ef_max` test hook.
        let cap = ef_max();
        let visible = mask.len();
        let mut ef = k.saturating_add(VECTOR_RESCORE_MARGIN);
        if visible > 0 && n > 0 {
            let over = k
                .saturating_mul(n)
                .div_ceil(visible)
                .saturating_add(VECTOR_RESCORE_MARGIN);
            ef = ef.max(over);
        }
        loop {
            let hits = match label {
                Some(lbl) => self
                    .engine
                    .hnsw_search_dst_with_ef(field, lbl, q_unit, ef, ef),
                None => self
                    .engine
                    .hnsw_search_any_dst_with_ef(field, q_unit, ef, ef),
            };
            let hits = hits?;
            let full = hits.len() == ef;
            let mut out = self.score_masked_hnsw_hits(&hits, field, q_unit, min, mask);
            if out.len() >= k {
                out.truncate(k);
                return Some(out);
            }
            // Short of its width (frontier exhausted) or at the ceiling:
            // a wider beam reaches nothing new, so the scan answers.
            if !full || ef >= cap {
                return None;
            }
            ef = ef.saturating_mul(2);
        }
    }

    /// Say once, per `(field, label)` index and caller shape, that a masked
    /// search is answering approximately.
    ///
    /// A mask narrows *which nodes may be returned*. It does not choose a
    /// kernel — `exact=true` and a `where=` predicate do, and nothing else
    /// does. A caller who needed exact answers, passed `mask=` alone, and read
    /// the mask as a promise of exhaustiveness gets a correct-looking
    /// approximate answer and no signal at all; that is a silent wrong answer,
    /// and it has cost an integration team real time.
    ///
    /// The fix is a question, not a behaviour change. Making a mask imply
    /// `exact` would turn every existing masked caller's ANN into an O(n) GEMM
    /// without asking them, which is a worse trade than the ambiguity.
    ///
    /// Printed once per index for the reason the dimension-mismatch skip in
    /// `core_rules::hnsw` is: a line on every call is a line callers learn to
    /// scroll past.
    ///
    /// `caller` decides the advice. The same leg is reached from two signatures
    /// and only one of them has an `exact` argument to pass; see
    /// [`ExactnessCaller`].
    fn note_ambiguous_exactness(&self, field: &str, label: Option<&str>, caller: ExactnessCaller) {
        let entry = (field.to_string(), label.unwrap_or("").to_string(), caller);
        let first = match self.warned_ambiguous_exactness.lock() {
            Ok(mut seen) => seen.insert(entry),
            Err(poisoned) => poisoned.into_inner().insert(entry),
        };
        if !first {
            return;
        }
        AMBIGUOUS_EXACTNESS_WARNS.with(|c| c.set(c.get().saturating_add(1)));
        let which = match label {
            Some(lbl) => format!(" (label `{lbl}`)"),
            None => String::new(),
        };
        let subject = caller.subject();
        let advice = caller.advice();
        let line = format!(
            "mushroomdb: {subject} on field `{field}`{which} is answering \
             approximately. A mask narrows which nodes may be returned; it does not \
             change which kernel runs, and an index covers this field. For an exact \
             answer over the same visible candidate set, {advice} Further masked \
             searches of this shape on this index are silent."
        );
        AMBIGUOUS_EXACTNESS_LAST.with(|c| *c.borrow_mut() = Some(line.clone()));
        eprintln!("{line}");
    }

    /// `label ∩ mask ∩ holds(where)`. Index fast path when `label` is `Some`
    /// and `(label, where.field)` is enabled; otherwise scan with `visible()`.
    fn vector_candidates(
        view: &GraphView<'_>,
        label: Option<&str>,
        where_: Option<&PropPredicate>,
    ) -> Vec<u32> {
        if let (Some(lbl), Some(pred)) = (label, where_) {
            let indexed = view
                .prop_index
                .is_some_and(|idx| idx.is_enabled(lbl, &pred.field));
            if indexed {
                match (&pred.eq, &pred.in_) {
                    (Some(eq), None) => {
                        if let Some(ids) = view.nodes_with_prop(lbl, &pred.field, eq) {
                            return ids;
                        }
                    }
                    (None, Some(allowed)) => {
                        let mut seen = HashSet::new();
                        let mut out = Vec::new();
                        for v in allowed {
                            if let Some(ids) = view.nodes_with_prop(lbl, &pred.field, v) {
                                for id in ids {
                                    if seen.insert(id) {
                                        out.push(id);
                                    }
                                }
                            }
                        }
                        return out;
                    }
                    _ => {}
                }
            }
        }

        let mut ids: Vec<u32> = match label {
            Some(lbl) => view
                .nodes_with_label(lbl)
                .into_iter()
                .filter(|&id| view.visible(id))
                .collect(),
            None => view.nodes_all(),
        };
        if let Some(pred) = where_ {
            ids.retain(|&id| match view.prop(id, &pred.field) {
                None => pred.holds(None),
                Some(vr) => pred.holds(Some(vr.as_value())),
            });
        }
        ids
    }

    /// Exact brute kNN: pack candidates at `q_unit`'s dim, GEMV, keep
    /// `score >= min`, sort `(sim desc, key asc)`, truncate to `k`.
    fn brute_vector_hits(
        &self,
        view: &GraphView<'_>,
        candidate_ids: impl IntoIterator<Item = u32>,
        field: &str,
        q_unit: &[f64],
        k: usize,
        min: f64,
    ) -> Vec<(String, f64)> {
        let rows: Vec<(u32, std::borrow::Cow<'_, [f64]>)> = candidate_ids
            .into_iter()
            .filter_map(|id| crate::exact_knn::vector_f64(view, id, field).map(|v| (id, v)))
            .collect();
        let packed =
            crate::exact_knn::pack(rows.iter().map(|(id, v)| (*id, v.as_ref())), q_unit.len());
        let scores = crate::exact_knn::gemv(&packed, q_unit);
        let mut scored: Vec<(String, f64)> = packed
            .ids
            .iter()
            .zip(scores.iter())
            .filter_map(|(&id, &sim)| {
                if sim < min {
                    return None;
                }
                let key = self.ids.key_of(id)?.to_string();
                Some((key, sim))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(k);
        scored
    }

    /// Exact cosine top-k for each key in `keys`, scored only against `keys`.
    ///
    /// `min` is cosine similarity in [-1, 1], inclusive (`score >= min`), the
    /// same unit and inequality as `find_similar_vector`. Self-matches are
    /// excluded. Unknown keys, keys with no `field`, zero-norm or wrong-dim
    /// embeddings are omitted as both query and candidate. Duplicate keys are
    /// collapsed, first-seen order. Empty `keys` → empty `Ok(vec![])`. Never
    /// uses HNSW. `n > PAIRWISE_MAX_N` → `QueryError`.
    #[allow(clippy::type_complexity)]
    pub fn pairwise_similar(
        &self,
        keys: &[&str],
        field: &str,
        k: usize,
        min: f64,
    ) -> Result<Vec<(String, Vec<(String, f64)>)>> {
        let mut seen = HashSet::new();
        let mut unique_ids = Vec::new();
        for key in keys {
            let Some(id) = self.ids.get(key) else {
                continue;
            };
            if seen.insert(id) {
                unique_ids.push(id);
            }
        }
        let max_n = crate::exact_knn::pairwise_max_n();
        if unique_ids.len() > max_n {
            return Err(GraphError::QueryError {
                detail: format!(
                    "pairwise_similar: n={} exceeds PAIRWISE_MAX_N ({max_n})",
                    unique_ids.len()
                ),
            });
        }
        if unique_ids.is_empty() {
            return Ok(Vec::new());
        }

        let view = self.view();
        let mut rows: Vec<(u32, std::borrow::Cow<'_, [f64]>)> = Vec::new();
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for id in unique_ids {
            let Some(v) = crate::exact_knn::vector_f64(&view, id, field) else {
                continue;
            };
            let norm: f64 = v.iter().map(|x| x * x).sum::<f64>().sqrt();
            if norm == 0.0 {
                continue;
            }
            *counts.entry(v.len()).or_default() += 1;
            rows.push((id, v));
        }
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let dim = counts
            .into_iter()
            .max_by_key(|&(d, c)| (c, d))
            .map(|(d, _)| d)
            .expect("rows non-empty");
        let packed = crate::exact_knn::pack(rows.iter().map(|(id, v)| (*id, v.as_ref())), dim);
        let n = packed.ids.len();
        if n == 0 {
            return Ok(Vec::new());
        }
        let src_keys: Vec<String> = packed
            .ids
            .iter()
            .map(|&id| self.ids.key_of(id).unwrap_or("").to_string())
            .collect();

        let mut out = Vec::with_capacity(n);
        if n <= crate::exact_knn::pairwise_gram_max() {
            let sims = crate::exact_knn::gram(&packed);
            for i in 0..n {
                out.push(Self::topk_from_row(
                    &src_keys,
                    i,
                    &sims[i * n..(i + 1) * n],
                    k,
                    min,
                ));
            }
        } else {
            for i in 0..n {
                let row = &packed.data[i * packed.dim..(i + 1) * packed.dim];
                let scores = crate::exact_knn::gemv(&packed, row);
                out.push(Self::topk_from_row(&src_keys, i, &scores, k, min));
            }
        }
        Ok(out)
    }

    /// [`pairwise_similar`](Self::pairwise_similar) over the keys the mask
    /// admits — intersected **before** the matmul, never filtered after it.
    ///
    /// A hidden vector packed into the Gram is a row every visible key is
    /// scored against. It can take a visible neighbour's place in the top-`k`,
    /// and because the packed dimension is a majority vote over the candidate
    /// rows it can decide whether a visible pair is scored at all. Dropping
    /// hidden names from the finished answer leaves both effects standing, so
    /// the intersection happens first and the answer is byte-for-byte the one
    /// `pairwise_similar` gives for the visible keys alone.
    ///
    /// The caps therefore measure the **post-filter** count: a key set over
    /// [`PAIRWISE_MAX_N`](crate::PAIRWISE_MAX_N) unscoped can come under it
    /// scoped and succeed, because the work the cap refuses is work this call
    /// no longer does. A filtered count still over the cap is still refused.
    #[allow(clippy::type_complexity)]
    pub fn pairwise_similar_scoped(
        &self,
        keys: &[&str],
        field: &str,
        k: usize,
        min: f64,
        mask: &crate::mask::NodeMask,
    ) -> Result<Vec<(String, Vec<(String, f64)>)>> {
        let visible: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|key| mask.contains_node(self, key))
            .collect();
        self.pairwise_similar(&visible, field, k, min)
    }

    /// Neighbours of packed row `i`: drop self, keep `score >= min`, sort
    /// `(sim desc, key asc)`, truncate to `k`. Packed srcs with no survivors
    /// still appear as `(src, [])`.
    fn topk_from_row(
        src_keys: &[String],
        i: usize,
        scores: &[f64],
        k: usize,
        min: f64,
    ) -> (String, Vec<(String, f64)>) {
        let mut neigh: Vec<(String, f64)> = scores
            .iter()
            .enumerate()
            .filter_map(|(j, &sim)| {
                if i == j || sim < min {
                    return None;
                }
                Some((src_keys[j].clone(), sim))
            })
            .collect();
        neigh.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        neigh.truncate(k);
        (src_keys[i].clone(), neigh)
    }

    /// Re-score HNSW candidates from the `f64` vectors, drop hidden / below-`min`
    /// hits, order by score then key. The index's own `f32` similarity is discarded.
    fn score_masked_hnsw_hits(
        &self,
        hits: &[(u32, f64)],
        field: &str,
        q_unit: &[f64],
        min: f64,
        mask: &crate::mask::NodeMask,
    ) -> Vec<(String, f64)> {
        let view = self.view_masked(mask);
        let mut out: Vec<(String, f64)> = hits
            .iter()
            .copied()
            .filter(|&(id, _)| mask.contains_id(id))
            .filter_map(|(id, _)| {
                let sim = exact_vector_similarity(&view, id, field, q_unit)?;
                if sim < min {
                    return None;
                }
                Some((self.ids.key_of(id)?.to_string(), sim))
            })
            .collect();
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        out
    }

    /// Read a single property from an edge.
    ///
    /// Returns `None` when the edge does not exist, the field is absent, or any
    /// of the string keys cannot be resolved to interned ids.  Only edge props
    /// written by rules (weight fields) are accessible without a `set_edge_prop`
    /// binding; topology-only edges (no props set) return `None` for every field.
    pub fn get_edge_prop(
        &self,
        edge_type: &str,
        src_key: &str,
        dst_key: &str,
        field: &str,
    ) -> Option<Value> {
        let etype = self.syms.get(edge_type)?;
        let src = self.ids.get(src_key)?;
        let dst = self.ids.get(dst_key)?;
        self.edge_props_view().get(etype, src, dst, field)
    }

    /// Lex → parse → plan → execute `cypher` over a read-only view.
    /// Every pipeline `Err(String)` becomes `GraphError::QueryError` with a
    /// stage prefix (`lex:` / `parse:` / `plan:` / `execute:`).
    pub fn query(&self, cypher: &str, params: &BTreeMap<String, Value>) -> Result<ResultSet> {
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        // `SHOW INDEXES` / `SHOW CONSTRAINTS` are reads; a statement that
        // changes the schema is a write, and this entry point does not write.
        if let Some(stmt) = ddl_statement(&tokens)? {
            if !stmt.is_show() {
                return Err(schema_change_on_a_read());
            }
            return Ok(self.schema_listing().show(&stmt));
        }
        if let Some(call) = call_statement(&tokens)? {
            return run_call(&call, &self.view(), &self.schema_listing(), params);
        }
        let union = parse_read(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        let t0 = std::time::Instant::now();
        let result = execute_union(&self.view(), &union, &Params(params)).map_err(|e| {
            GraphError::QueryError {
                detail: format!("execute: {e}"),
            }
        });
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        self.slow_query_sink()
            .record(elapsed_ms, cypher, self.commit_seq);
        result
    }

    /// This handle's slow-query threshold and ring, shareable with a snapshot.
    pub(crate) fn slow_query_sink(&self) -> SlowQuerySink {
        SlowQuerySink {
            threshold_ms: self.slow_query_threshold_ms,
            log: Arc::clone(&self.slow_queries),
        }
    }

    /// Convenience entry-point that accepts a slice of `(name, value)` pairs
    /// instead of a pre-built `BTreeMap`.  Equivalent to building the map and
    /// calling [`GraphDb::query`].
    pub fn query_with_params(&self, cypher: &str, params: &[(&str, Value)]) -> Result<ResultSet> {
        let map: BTreeMap<String, Value> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        self.query(cypher, &map)
    }

    /// Execute a Cypher write statement (CREATE / MATCH…SET / MATCH…DELETE / MERGE).
    ///
    /// All mutations flow through the same `insert_node` / `set_prop` /
    /// `delete_edge` / `insert_edge` path as the Rust API so the rule engine
    /// fires and the WAL captures everything with one fsync per statement.
    ///
    /// Returns a one-row [`ResultSet`] with columns `created`, `properties_set`,
    /// and `deleted` matching the write-result contract.
    ///
    /// **Mutation routing**: mutations are collected into a single
    /// [`BatchBuilder`] and committed atomically (one WAL `Batch` frame, one
    /// fsync). The MATCH phase for SET/DELETE uses a read-only `execute` call
    /// over `self.view()` — the borrow is dropped before the batch is opened.
    ///
    /// **Limitations (v1)**:
    /// - SET RHS must be a literal, `$param`, or arithmetic; bare property copy → named error.
    /// - `DETACH DELETE n` → calls `delete_node` for each matched node (removes all edges).
    /// - Bare `DELETE n` → error if n has any incident edges; succeeds for isolated nodes.
    /// - MERGE supports `ON CREATE SET` / `ON MATCH SET` in the same write batch.
    /// - Deleting a derived edge → named error "cannot delete derived edge".
    pub fn query_write(
        &mut self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        if let Some(stmt) = ddl_statement(&tokens)? {
            return self.exec_ddl(stmt);
        }
        // `CALL` is a read; the write entry point answers it as `query` does.
        if let Some(call) = call_statement(&tokens)? {
            return run_call(&call, &self.view(), &self.schema_listing(), params);
        }
        let stmt = parse_write(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        self.exec_write_stmt(stmt, params)
    }

    fn exec_write_stmt(
        &mut self,
        stmt: WriteStatement,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        match stmt {
            WriteStatement::Create(s) => self.exec_create(s, params),
            WriteStatement::MatchSet(s) => self.exec_match_set(s, params),
            WriteStatement::MatchSetMap(s) => self.exec_match_set_map(s, params),
            WriteStatement::MatchRemove(s) => self.exec_match_remove(s, params),
            WriteStatement::MatchDelete(s) => self.exec_match_delete(s, params),
            WriteStatement::MatchDeleteNode(s) => self.exec_match_delete_node(s, params),
            WriteStatement::Merge(s) => self.exec_merge(s, params),
            WriteStatement::UnwindWrite(s) => self.exec_unwind_write(*s, params),
        }
    }

    fn exec_create(
        &mut self,
        stmt: core_query::cypher::CreateStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // Extract the node key from props: require a string-valued `id` field.
        let mut var_to_key: BTreeMap<String, String> = BTreeMap::new();
        for node in &stmt.nodes {
            let var = node.var.as_deref().unwrap_or("_cn0");
            let key = node
                .props
                .iter()
                .find(|(f, _)| f == "id")
                .and_then(|(_, v)| {
                    if let Value::Str(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .ok_or_else(|| GraphError::QueryError {
                    detail: format!(
                        "CREATE node ({}:{}) requires a string 'id' property",
                        var, node.label
                    ),
                })?;
            var_to_key.insert(var.to_string(), key);
        }

        let mut batch = self.batch();
        let mut created: usize = 0;
        for node in &stmt.nodes {
            let var = node.var.as_deref().unwrap_or("_cn0");
            let key = &var_to_key[var];
            batch.insert_node(&node.label, key, node.props.clone());
            created += 1;
        }
        for edge in &stmt.edges {
            let src_key = var_to_key
                .get(&edge.src_var)
                .ok_or_else(|| GraphError::QueryError {
                    detail: format!("CREATE edge src variable '{}' is not bound", edge.src_var),
                })?;
            let dst_key = var_to_key
                .get(&edge.dst_var)
                .ok_or_else(|| GraphError::QueryError {
                    detail: format!("CREATE edge dst variable '{}' is not bound", edge.dst_var),
                })?;
            batch.insert_edge(&edge.etype, src_key, dst_key);
        }
        batch.commit()?;

        // Optional RETURN clause: project created bindings as a read result.
        if let Some(returns) = stmt.returns {
            // Each created node is looked up by its key via a separate MATCH pattern.
            // Multiple single-node patterns cross-join to produce 1 output row with
            // all variables bound (each pattern returns exactly 1 row).
            let patterns: Vec<Pattern> = stmt
                .nodes
                .iter()
                .map(|node| {
                    let var = node.var.as_deref().unwrap_or("_cn0");
                    let key = var_to_key[var].clone();
                    Pattern {
                        start: NodePat {
                            var: Some(var.to_string()),
                            label: Some(node.label.clone()),
                            props: vec![("id".to_string(), Operand::Lit(Value::Str(key)))],
                        },
                        chain: vec![],
                        shortest: false,
                        path_var: None,
                        continues_match: false,
                    }
                })
                .collect();
            let q = Query {
                matches: patterns,
                optional_clauses: vec![],
                where_expr: None,
                unwinds: vec![],
                post_unwind_where: None,
                stages: vec![],
                returns,
                distinct: false,
                order_by: vec![],
                skip: None,
                limit: None,
            };
            let ops = plan(&q).map_err(|e| GraphError::QueryError {
                detail: format!("plan: {e}"),
            })?;
            return execute(&self.view(), &ops, &Params(params)).map_err(|e| {
                GraphError::QueryError {
                    detail: format!("execute: {e}"),
                }
            });
        }

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(created as i64)),
            Some(Value::Int(0)),
            Some(Value::Int(0)),
        ]);
        Ok(rs)
    }

    fn exec_match_set(
        &mut self,
        stmt: core_query::cypher::MatchSetStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let project_returns = stmt.returns.clone();
        // Refused before anything is written: the projection runs after the
        // commit, and must not fail (or differ) once the write has happened.
        if let Some(detail) = project_returns
            .as_deref()
            .and_then(returns_write_return_refusal)
        {
            return Err(GraphError::QueryError {
                detail: detail.into(),
            });
        }
        // Collect unique node vars targeted by SET clauses, plus RETURN bindings
        // so the post-write projection can look them up by key.
        let mut set_vars: Vec<String> = Vec::new();
        for s in &stmt.sets {
            if !set_vars.contains(&s.var) {
                set_vars.push(s.var.clone());
            }
        }
        let rel_vars = pattern_rel_vars(&stmt.matches);
        // `count` is the engine's, on an edge: it is the insert-count §5.13
        // maintains, and a `SET` that overwrote it would make the number mean
        // whatever the last writer said rather than how many times the pair was
        // inserted. Refused by name here, before the match runs, so the caller
        // is told what is actually wrong instead of meeting the executor's
        // generic "did not resolve to a node key" — and so the answer does not
        // depend on whether the pattern happened to match a row. The same name
        // on a *node* is an ordinary property and is untouched.
        for s in &stmt.sets {
            if s.field == EDGE_COUNT_PROP && rel_vars.iter().any(|r| r == &s.var) {
                return Err(GraphError::QueryError {
                    detail: format!(
                        "cannot SET {}.{EDGE_COUNT_PROP}: `{EDGE_COUNT_PROP}` is a reserved edge \
                         property holding the pair's insert count",
                        s.var
                    ),
                });
            }
        }
        let mut lookup_vars = set_vars.clone();
        for v in pattern_node_vars(&stmt.matches) {
            add_var(&mut lookup_vars, &v);
        }
        if let Some(ref returns) = project_returns {
            for v in ret_node_vars(returns) {
                if !rel_vars.iter().any(|r| r == &v) {
                    add_var(&mut lookup_vars, &v);
                }
            }
        }

        // Synthesize a read query: MATCH … WHERE … RETURN <lookup_vars>, <set_values…>
        // SET values are projected as ScalarExpr items so that arithmetic expressions
        // (e.g. `SET n.score = n.score * 1.5`) are evaluated in the matched-row context.
        let mut set_returns: Vec<RetItem> = lookup_vars
            .iter()
            .map(|v| RetItem {
                value: RetVal::Var(v.clone()),
                alias: None,
            })
            .collect();
        // One computed column per SET clause; alias is `__sv_<i>`.
        let set_val_cols: Vec<String> = stmt
            .sets
            .iter()
            .enumerate()
            .map(|(i, _)| format!("__sv_{i}"))
            .collect();
        for (sc, col) in stmt.sets.iter().zip(&set_val_cols) {
            set_returns.push(RetItem {
                value: RetVal::ScalarExpr(sc.value.clone()),
                alias: Some(col.clone()),
            });
        }
        // Capture relationship types while r is bound; SET does not change them.
        for r in &rel_vars {
            set_returns.push(RetItem {
                value: RetVal::FuncCall {
                    name: "type".into(),
                    args: vec![Operand::Var(r.clone())],
                },
                alias: Some(rel_type_alias(r)),
            });
        }

        let read_q = Query {
            matches: stmt.matches.clone(),
            optional_clauses: vec![],
            where_expr: stmt.where_expr.clone(),
            unwinds: vec![],
            post_unwind_where: None,
            stages: vec![],
            returns: set_returns,
            distinct: false,
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        // MATCH phase is read-only; borrow ends before batch opens.
        //
        // When a role-scoped write is in flight, run the MATCH read through
        // view_masked so hidden nodes are invisible → hidden ≡ absent ≡
        // zero-rows (no SetProp ops generated, no existence-oracle 403).
        // Full-authority writes (pending_write_authz=None) keep view().
        let match_rs = {
            let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
            if let Some(ref mask) = mask_opt {
                execute(&self.view_masked(mask), &ops, &Params(params))
            } else {
                execute(&self.view(), &ops, &Params(params))
            }
        }
        .map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })?;

        // Collect (key, field, value) for each matched row × each SET clause.
        //
        // A value that reads a node's properties is evaluated as its turn comes,
        // against what the statement has written so far, so two rows for one
        // node (`MATCH (n), (m) SET n.c = n.c + 1`) and two items of one clause
        // (`SET n.v = 10, n.w = n.v`) read each other's writes, as in openCypher.
        // A value with no such read keeps its column from the MATCH.
        let node_names = pattern_node_vars(&stmt.matches);
        let dynamic: Vec<bool> = stmt
            .sets
            .iter()
            .map(|sc| {
                row_eval::reads_node(&sc.value, &node_names)
                    && !row_eval::reads_node(&sc.value, &rel_vars)
            })
            .collect();
        let any_dynamic = dynamic.iter().any(|d| *d);
        let mut overlay = row_eval::Overlay::default();
        let mut set_ops: Vec<(String, String, Value)> = Vec::new();
        for row_i in 0..match_rs.len() {
            for ((sc, col), is_dynamic) in stmt.sets.iter().zip(&set_val_cols).zip(&dynamic) {
                let key = match match_rs.get(row_i, &sc.var) {
                    Some(Value::Str(k)) => k.clone(),
                    _ => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "SET variable '{}' did not resolve to a node key",
                                sc.var
                            ),
                        })
                    }
                };
                let evaluated = if *is_dynamic {
                    let node_key = |name: &str| -> Option<Option<String>> {
                        node_names.iter().any(|n| n == name).then(|| {
                            match match_rs.get(row_i, name) {
                                Some(Value::Str(k)) => Some(k.clone()),
                                _ => None,
                            }
                        })
                    };
                    self.uw_eval(&mut overlay, &sc.value, &node_key, &[], params)?
                } else {
                    // The SET value was already evaluated by the executor.
                    match_rs.get(row_i, col).cloned()
                };
                let Some(value) = evaluated.filter(|v| !matches!(v, Value::Null)) else {
                    return Err(GraphError::QueryError {
                        detail: format!("SET value for {}.{} evaluated to null", sc.var, sc.field),
                    });
                };
                if any_dynamic {
                    overlay.note_set(&key, &sc.field, &value);
                }
                set_ops.push((key, sc.field.clone(), value));
            }
        }

        // Apply as one atomic batch.
        let props_set = set_ops.len();
        let mut batch = self.batch();
        for (key, field, value) in set_ops {
            batch.set_prop(&key, &field, value);
        }
        batch.commit()?;

        if let Some(returns) = project_returns {
            return project_set_return_rows(self, &rel_vars, &match_rs, &returns, params);
        }

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(0)),
            Some(Value::Int(props_set as i64)),
            Some(Value::Int(0)),
        ]);
        Ok(rs)
    }

    /// `MATCH … [WHERE …] SET n = {…}` and `SET n += {…}`.
    ///
    /// The MATCH runs first (masked under a role-scoped write); each map value
    /// is evaluated per matched row by the same executor that evaluates
    /// `SET n.f = expr`. The resulting `SetProp`/`RemoveProp` ops commit as one
    /// batch, so they meet the same choke-point as every other write: TY-5
    /// constraints, `ns` immutability, the view-owned refusal, and rule
    /// re-evaluation for each removal. See [`plan_map_write`] for which
    /// properties each form touches.
    fn exec_match_set_map(
        &mut self,
        stmt: core_query::cypher::MatchSetMapStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let project_returns = stmt.returns.clone();
        let rel_vars = pattern_rel_vars(&stmt.matches);
        let mut lookup_vars: Vec<String> = Vec::new();
        add_var(&mut lookup_vars, &stmt.var);
        for v in pattern_node_vars(&stmt.matches) {
            add_var(&mut lookup_vars, &v);
        }
        if let Some(ref returns) = project_returns {
            for v in ret_node_vars(returns) {
                if !rel_vars.iter().any(|r| r == &v) {
                    add_var(&mut lookup_vars, &v);
                }
            }
        }
        let mut match_returns: Vec<RetItem> = lookup_vars
            .iter()
            .map(|v| RetItem {
                value: RetVal::Var(v.clone()),
                alias: None,
            })
            .collect();
        let value_cols: Vec<String> = (0..stmt.map.len()).map(|i| format!("__sv_{i}")).collect();
        for ((_, operand), col) in stmt.map.iter().zip(&value_cols) {
            match_returns.push(RetItem {
                value: RetVal::ScalarExpr(operand.clone()),
                alias: Some(col.clone()),
            });
        }
        if let Some(name) = &stmt.param {
            match_returns.push(RetItem {
                value: RetVal::ScalarExpr(Operand::Param(name.clone())),
                alias: Some("__sv_param".into()),
            });
        }
        for r in &rel_vars {
            match_returns.push(RetItem {
                value: RetVal::FuncCall {
                    name: "type".into(),
                    args: vec![Operand::Var(r.clone())],
                },
                alias: Some(rel_type_alias(r)),
            });
        }
        let read_q = Query {
            matches: stmt.matches.clone(),
            optional_clauses: vec![],
            where_expr: stmt.where_expr.clone(),
            unwinds: vec![],
            post_unwind_where: None,
            stages: vec![],
            returns: match_returns,
            distinct: false,
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        let match_rs = {
            let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
            if let Some(ref mask) = mask_opt {
                execute(&self.view_masked(mask), &ops, &Params(params))
            } else {
                execute(&self.view(), &ops, &Params(params))
            }
        }
        .map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })?;

        // One entry list per matched row, in row order. A node bound by several
        // rows is written by each in turn under `+=`; under `=` the last row's
        // map is the node's final state, so earlier rows for it are dropped.
        let node_names = pattern_node_vars(&stmt.matches);
        let map_dynamic = stmt.map.iter().any(|(_, operand)| {
            row_eval::reads_node(operand, &node_names) && !row_eval::reads_node(operand, &rel_vars)
        });
        let mut overlay = row_eval::Overlay::default();
        let mut per_node: Vec<(String, MapEntries)> = Vec::new();
        for row_i in 0..match_rs.len() {
            let key = match match_rs.get(row_i, &stmt.var) {
                Some(Value::Str(k)) => k.clone(),
                _ => {
                    return Err(GraphError::QueryError {
                        detail: format!(
                            "SET variable '{}' did not resolve to a node key",
                            stmt.var
                        ),
                    })
                }
            };
            if let Some(name) = &stmt.param {
                // The parameter is a map: its keys are the map's, a null value
                // is a null entry. Anything else is a named error, before any
                // write.
                let entries = match match_rs.get(row_i, "__sv_param") {
                    Some(Value::Map(m)) => m
                        .iter()
                        .map(|(k, v)| {
                            let v = if matches!(v, Value::Null) {
                                None
                            } else {
                                Some(v.clone())
                            };
                            (k.clone(), v)
                        })
                        .collect::<MapEntries>(),
                    other => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "SET {} {}= ${name}: the parameter must be a map, got {}",
                                stmt.var,
                                if stmt.merge { "+" } else { "" },
                                other.map_or("null".to_string(), |v| format!("{v:?}")),
                            ),
                        })
                    }
                };
                if !stmt.merge {
                    per_node.retain(|(k, _)| k != &key);
                }
                per_node.push((key, entries));
                continue;
            }
            let mut entries: MapEntries = Vec::with_capacity(stmt.map.len());
            for ((field, operand), col) in stmt.map.iter().zip(&value_cols) {
                let value = if row_eval::reads_node(operand, &node_names)
                    && !row_eval::reads_node(operand, &rel_vars)
                {
                    // Read as its turn comes, against the statement's own writes.
                    let node_key = |name: &str| -> Option<Option<String>> {
                        node_names.iter().any(|n| n == name).then(|| {
                            match match_rs.get(row_i, name) {
                                Some(Value::Str(k)) => Some(k.clone()),
                                _ => None,
                            }
                        })
                    };
                    self.uw_eval(&mut overlay, operand, &node_key, &[], params)?
                } else {
                    match match_rs.get(row_i, col) {
                        None | Some(Value::Null) => None,
                        Some(v) => Some(v.clone()),
                    }
                };
                entries.push((field.clone(), value));
            }
            if map_dynamic {
                self.uw_apply_map(&mut overlay, &key, stmt.merge, &entries);
            }
            if !stmt.merge {
                per_node.retain(|(k, _)| k != &key);
            }
            per_node.push((key, entries));
        }

        let mut writes: Vec<(String, MapWrite)> = Vec::new();
        let mut props_changed = 0i64;
        for (key, entries) in &per_node {
            let existing = self.node_info(key).map(|n| n.props).unwrap_or_default();
            let node_writes = plan_map_write(key, &existing, stmt.merge, entries, &|field| {
                self.view_store.view_for_prop(field).is_some()
            });
            for write in node_writes {
                match &write {
                    MapWrite::Set { .. } => props_changed += 1,
                    MapWrite::Remove { field } => {
                        if existing.contains_key(field) {
                            props_changed += 1;
                        }
                    }
                }
                writes.push((key.clone(), write));
            }
        }
        let mut batch = self.batch();
        for (key, write) in writes {
            match write {
                MapWrite::Set { field, value } => {
                    batch.set_prop(&key, &field, value);
                }
                MapWrite::Remove { field } => {
                    batch.remove_prop(&key, &field);
                }
            }
        }
        batch.commit()?;

        if let Some(returns) = project_returns {
            return project_set_return_rows(self, &rel_vars, &match_rs, &returns, params);
        }
        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(0)),
            Some(Value::Int(props_changed)),
            Some(Value::Int(0)),
        ]);
        Ok(rs)
    }

    /// `MATCH … [WHERE …] REMOVE n.f [, …] [RETURN …]`.
    ///
    /// The MATCH runs first (masked under a role-scoped write, so a hidden node
    /// is absent and nothing is removed from it); the removals then commit as
    /// one batch of `RemoveProp` ops, which meet the same `prepare_remove_prop`
    /// choke-point as `remove_prop`: view-owned fields and `ns` are refused there,
    /// a field the node lacks is a no-op, and rules re-evaluate each removal.
    /// The result row's `properties_set` slot counts the properties that existed
    /// and were removed.
    fn exec_match_remove(
        &mut self,
        stmt: core_query::cypher::MatchRemoveStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let project_returns = stmt.returns.clone();
        let rel_vars = pattern_rel_vars(&stmt.matches);
        let mut lookup_vars: Vec<String> = Vec::new();
        for (var, _) in &stmt.removals {
            add_var(&mut lookup_vars, var);
        }
        for v in pattern_node_vars(&stmt.matches) {
            add_var(&mut lookup_vars, &v);
        }
        if let Some(ref returns) = project_returns {
            for v in ret_node_vars(returns) {
                if !rel_vars.iter().any(|r| r == &v) {
                    add_var(&mut lookup_vars, &v);
                }
            }
        }
        let mut match_returns: Vec<RetItem> = lookup_vars
            .iter()
            .map(|v| RetItem {
                value: RetVal::Var(v.clone()),
                alias: None,
            })
            .collect();
        // Capture relationship types while r is bound, as SET does.
        for r in &rel_vars {
            match_returns.push(RetItem {
                value: RetVal::FuncCall {
                    name: "type".into(),
                    args: vec![Operand::Var(r.clone())],
                },
                alias: Some(rel_type_alias(r)),
            });
        }
        let read_q = Query {
            matches: stmt.matches.clone(),
            optional_clauses: vec![],
            where_expr: stmt.where_expr.clone(),
            unwinds: vec![],
            post_unwind_where: None,
            stages: vec![],
            returns: match_returns,
            distinct: false,
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        let match_rs = {
            let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
            if let Some(ref mask) = mask_opt {
                execute(&self.view_masked(mask), &ops, &Params(params))
            } else {
                execute(&self.view(), &ops, &Params(params))
            }
        }
        .map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })?;

        // Each (node, field) once, however many rows bound the node.
        let mut targets: Vec<(String, String)> = Vec::new();
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for row_i in 0..match_rs.len() {
            for (var, field) in &stmt.removals {
                let key = match match_rs.get(row_i, var) {
                    Some(Value::Str(k)) => k.clone(),
                    _ => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "REMOVE variable '{var}' did not resolve to a node key"
                            ),
                        })
                    }
                };
                let pair = (key, field.clone());
                if seen.insert(pair.clone()) {
                    targets.push(pair);
                }
            }
        }

        // Counted before the commit, from the state the removals act on.
        let props_removed = targets
            .iter()
            .filter(|(key, field)| self.get_prop(key, field).is_some())
            .count() as i64;
        let mut batch = self.batch();
        for (key, field) in &targets {
            batch.remove_prop(key, field);
        }
        batch.commit()?;

        if let Some(returns) = project_returns {
            return project_set_return_rows(self, &rel_vars, &match_rs, &returns, params);
        }

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(0)),
            Some(Value::Int(props_removed)),
            Some(Value::Int(0)),
        ]);
        Ok(rs)
    }

    fn exec_match_delete(
        &mut self,
        stmt: core_query::cypher::MatchDeleteStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // Collect unique node vars needed to identify edge endpoints.
        let mut node_vars: Vec<String> = Vec::new();
        for ed in &stmt.deletes {
            if !node_vars.contains(&ed.src_var) {
                node_vars.push(ed.src_var.clone());
            }
            if !node_vars.contains(&ed.dst_var) {
                node_vars.push(ed.dst_var.clone());
            }
        }

        // Synthesize read query.
        let returns: Vec<RetItem> = node_vars
            .iter()
            .map(|v| RetItem {
                value: RetVal::Var(v.clone()),
                alias: None,
            })
            .collect();
        let read_q = Query {
            matches: stmt.matches,
            optional_clauses: vec![],
            where_expr: stmt.where_expr,
            unwinds: vec![],
            post_unwind_where: None,
            stages: vec![],
            returns,
            distinct: false,
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        // Role-scoped writes: mask the MATCH read phase so hidden nodes are
        // invisible → hidden ≡ absent ≡ zero-rows (spec §3.1, hidden ≡ absent).
        let match_rs = {
            let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
            if let Some(ref mask) = mask_opt {
                execute(&self.view_masked(mask), &ops, &Params(params))
            } else {
                execute(&self.view(), &ops, &Params(params))
            }
        }
        .map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })?;

        // Collect (etype, src_key, dst_key) for each row × each delete target.
        let mut del_ops: Vec<(String, String, String)> = Vec::new();
        for row_i in 0..match_rs.len() {
            for ed in &stmt.deletes {
                let src_key = match match_rs.get(row_i, &ed.src_var) {
                    Some(Value::Str(k)) => k.clone(),
                    _ => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "DELETE src variable '{}' did not resolve to a node key",
                                ed.src_var
                            ),
                        })
                    }
                };
                let dst_key = match match_rs.get(row_i, &ed.dst_var) {
                    Some(Value::Str(k)) => k.clone(),
                    _ => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "DELETE dst variable '{}' did not resolve to a node key",
                                ed.dst_var
                            ),
                        })
                    }
                };
                del_ops.push((ed.etype.clone(), src_key, dst_key));
            }
        }

        // Apply as one atomic batch.
        let deleted = del_ops.len();
        let mut batch = self.batch();
        for (etype, src_key, dst_key) in del_ops {
            batch.delete_edge(&etype, &src_key, &dst_key);
        }
        batch.commit().map_err(|e| match e {
            GraphError::RuleOwned { .. } => GraphError::QueryError {
                detail: "cannot delete derived edge; retract via the rule or change the property"
                    .to_string(),
            },
            other => other,
        })?;

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(0)),
            Some(Value::Int(0)),
            Some(Value::Int(deleted as i64)),
        ]);
        Ok(rs)
    }

    /// Execute `MATCH … [DETACH] DELETE <node_var> [, …]`.
    ///
    /// Collects the matching node keys via an ephemeral read query, then calls
    /// `delete_node` on each one.  When `stmt.detach` is `false` (bare DELETE)
    /// the executor first checks that the node has no incident edges; if any
    /// remain it returns a named error matching openCypher semantics.
    fn exec_match_delete_node(
        &mut self,
        stmt: MatchDeleteNodeStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // Build a read query returning only the node keys we need.
        let returns: Vec<RetItem> = stmt
            .node_vars
            .iter()
            .map(|v| RetItem {
                value: RetVal::Var(v.clone()),
                alias: None,
            })
            .collect();
        let read_q = Query {
            matches: stmt.matches,
            optional_clauses: vec![],
            where_expr: stmt.where_expr,
            unwinds: vec![],
            post_unwind_where: None,
            stages: vec![],
            returns,
            distinct: false,
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        // Role-scoped writes: mask the MATCH read phase so hidden nodes are
        // invisible → hidden ≡ absent ≡ zero-rows (spec §3.1, hidden ≡ absent).
        let match_rs = {
            let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
            if let Some(ref mask) = mask_opt {
                execute(&self.view_masked(mask), &ops, &Params(params))
            } else {
                execute(&self.view(), &ops, &Params(params))
            }
        }
        .map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })?;

        // Collect unique node keys to delete (deduplicate across rows × vars).
        let mut keys: Vec<String> = Vec::new();
        for row_i in 0..match_rs.len() {
            for var in &stmt.node_vars {
                if let Some(Value::Str(k)) = match_rs.get(row_i, var) {
                    if !keys.contains(k) {
                        keys.push(k.clone());
                    }
                }
            }
        }

        if !stmt.detach {
            // openCypher bare DELETE: error if any matched node has incident edges.
            for key in &keys {
                if let Some(id) = self.ids.get(key) {
                    let tv = self.topo_view();
                    let has_edges = tv.etypes().any(|et| {
                        !tv.neighbors(et, Direction::Out, id).is_empty()
                            || !tv.neighbors(et, Direction::In, id).is_empty()
                    });
                    if has_edges {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "Cannot delete node `{key}` because it still has incident edges. \
                                 Use DETACH DELETE to remove the node and all its edges."
                            ),
                        });
                    }
                }
            }
        }

        let mut nodes_deleted = 0i64;
        let mut edges_deleted = 0i64;
        for key in keys {
            match self.delete_node(&key) {
                Ok(report) => {
                    nodes_deleted += 1;
                    edges_deleted += (report.manual_edges + report.derived_edges) as i64;
                }
                Err(GraphError::KeyNotFound { .. }) => {
                    // Node may have been deleted by an earlier iteration (e.g., via
                    // multiple MATCH rows for the same node).  Safe to skip.
                }
                Err(e) => return Err(e),
            }
        }

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(0)),
            Some(Value::Int(0)),
            Some(Value::Int(nodes_deleted + edges_deleted)),
        ]);
        Ok(rs)
    }

    /// Props the MERGE create arm inserts: the identifying key, plus `ns` when
    /// the pattern named one, or the executing role's sole namespace when it
    /// did not. A role bound to two or more namespaces cannot choose, and is
    /// refused with [`MERGE_CREATE_NEEDS_ONE_NAMESPACE`]. The authorizer still
    /// refuses a named `ns` the role cannot write.
    fn merge_create_props(
        &self,
        key_field: &str,
        key_value: &Value,
        named_ns: Option<&Value>,
    ) -> Result<Vec<(String, Value)>> {
        let mut props = vec![(key_field.to_string(), key_value.clone())];
        if let Some(ns) = named_ns {
            props.push((NS_PROP.to_string(), ns.clone()));
            return Ok(props);
        }
        if let Some(ns) = self.merge_create_stamp_ns()? {
            props.push((NS_PROP.to_string(), Value::Str(ns)));
        }
        Ok(props)
    }

    /// The namespace a role-scoped MERGE create stamps when the pattern does
    /// not name `ns`. `None` = unscoped / full authority, so the node lands in
    /// `default`.
    fn merge_create_stamp_ns(&self) -> Result<Option<String>> {
        let Some(authz) = self.pending_write_authz.as_ref() else {
            return Ok(None);
        };
        let Some(def) = self.role_def_for(&authz.role) else {
            return Ok(None);
        };
        match def.namespaces.as_deref() {
            Some([only]) => Ok(Some(only.clone())),
            Some(_) => Err(GraphError::RoleWriteDenied {
                reason: MERGE_CREATE_NEEDS_ONE_NAMESPACE.to_string(),
            }),
            None => Ok(None),
        }
    }

    /// Whether the MERGE node `key` exists, after the role-scope check (when
    /// the write is role-scoped). See the comments inside for the oracle rules.
    fn merge_node_existed(&self, label: &str, key: &str) -> Result<bool> {
        if let Some(authz) = self.pending_write_authz.clone() {
            let has_create = authz.scope.create_labels.iter().any(|l| l == label);
            let has_update = authz.scope.update_labels.iter().any(|l| l == label);
            if !has_create && !has_update {
                // Scope-before-lookup: 403 without has_node call (timing oracle
                // closure — see test_merge_unscoped_no_key_lookup).
                return Err(GraphError::RoleWriteDenied {
                    reason: format!(
                        "role-bound token: label '{}' not in write scope (create_labels)",
                        label
                    ),
                });
            }
            // Key lookup under mask.
            match self.ids.get(key) {
                Some(id) if authz.mask.contains_id(id) => {
                    // Visible: must have update scope to proceed to match arm.
                    if !has_update {
                        return Err(GraphError::RoleWriteDenied {
                            reason: format!(
                                "role-bound token: label '{}' not in write scope (update_labels)",
                                label
                            ),
                        });
                    }
                    Ok(true) // existed = true → match arm
                }
                Some(_) => {
                    // Hidden: same error as absent to the role (spec §3.1/§3.3).
                    Err(GraphError::RoleWriteDenied {
                        reason: "role-bound token: target node not visible".into(),
                    })
                }
                None => {
                    // Absent: must have create scope to proceed to the create arm.
                    //
                    // Update-only roles (create_labels empty, update_labels set):
                    // return the SAME "not visible" error as the hidden-key branch
                    // so hidden ≡ absent — no distinguishing oracle (spec §6.1
                    // "confirm existence of hidden nodes: No").
                    //
                    // Create-scoped roles (has_create=true): absent → create arm
                    // as before.  The accepted structural key-existence disclosure
                    // (§THREAT-MODEL) applies only when the role holds create scope.
                    if !has_create {
                        return Err(GraphError::RoleWriteDenied {
                            reason: "role-bound token: target node not visible".into(),
                        });
                    }
                    Ok(false) // existed = false → create arm
                }
            }
        } else {
            // Full authority: use the existing non-masked has_node check.
            Ok(self.has_node(key))
        }
    }

    /// Whether the manual-or-derived edge `src -[etype]-> dst` is in the applied state.
    fn merge_edge_present(&self, edge_type: &str, src_key: &str, dst_key: &str) -> bool {
        self.ensure_v8_base_sections_loaded();
        let (Some(src), Some(dst), Some(sym)) = (
            self.ids.get(src_key),
            self.ids.get(dst_key),
            self.syms.get(edge_type),
        ) else {
            return false;
        };
        self.topo_view()
            .neighbors(sym, Direction::Out, src)
            .binary_search(&dst)
            .is_ok()
    }

    /// `MERGE (a:L {id: 'x'})-[:T]->(b:M {id: 'y'}) [ON CREATE SET …] [ON MATCH SET …] [RETURN …]`.
    ///
    /// Find-or-create each end node by key, then the edge between them. The
    /// pattern matched as a whole (both nodes and the edge existed) → `ON
    /// MATCH`; otherwise the missing nodes and the edge are created and `ON
    /// CREATE` runs. One batch, so one commit and one WAL frame; every write
    /// meets the usual commit-time checks.
    fn exec_merge_edge(
        &mut self,
        stmt: core_query::cypher::MergeStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let Some(edge) = stmt.edge.clone() else {
            return Err(GraphError::QueryError {
                detail: "MERGE relationship pattern missing".into(),
            });
        };
        let key_of = |v: &Value| match v {
            Value::Str(s) => Ok(s.clone()),
            other => Err(GraphError::QueryError {
                detail: format!("MERGE key value must be a string (got {other:?})"),
            }),
        };
        let start_key = key_of(&stmt.key_value)?;
        let end_key = key_of(&edge.end_key_value)?;
        if start_key == end_key && stmt.label != edge.end_label {
            return Err(GraphError::QueryError {
                detail: format!(
                    "MERGE: key '{start_key}' is given both label '{}' and label '{}'",
                    stmt.label, edge.end_label
                ),
            });
        }
        // A SET item names the start node or the end node, never the relationship.
        let set_target = |var: &str| -> Result<&str> {
            if stmt.var.as_deref() == Some(var) {
                Ok(start_key.as_str())
            } else if edge.end_var.as_deref() == Some(var) {
                Ok(end_key.as_str())
            } else {
                Err(GraphError::QueryError {
                    detail: format!(
                        "SET variable '{var}' does not match a MERGE node variable \
                         (a relationship takes no SET)"
                    ),
                })
            }
        };
        let mut create_sets = Vec::new();
        for sc in &stmt.on_create {
            create_sets.push((set_target(&sc.var)?, &sc.field, &sc.value));
        }
        let mut match_sets = Vec::new();
        for sc in &stmt.on_match {
            match_sets.push((set_target(&sc.var)?, &sc.field, &sc.value));
        }

        // Scope checks on both nodes before either key is looked up beyond them.
        let start_existed = self.merge_node_existed(&stmt.label, &start_key)?;
        let end_existed = if end_key == start_key {
            start_existed
        } else {
            self.merge_node_existed(&edge.end_label, &end_key)?
        };
        let (src_key, dst_key) = match edge.dir {
            core_query::cypher::RelDir::Left => (end_key.as_str(), start_key.as_str()),
            _ => (start_key.as_str(), end_key.as_str()),
        };
        let edge_existed =
            start_existed && end_existed && self.merge_edge_present(&edge.etype, src_key, dst_key);

        let start_create = if start_existed {
            None
        } else {
            Some(self.merge_create_props(&stmt.key_field, &stmt.key_value, stmt.ns.as_ref())?)
        };
        let end_create = if end_existed || end_key == start_key {
            None
        } else {
            Some(self.merge_create_props(
                &edge.end_key_field,
                &edge.end_key_value,
                edge.end_ns.as_ref(),
            )?)
        };
        let mut created = 0i64;
        let mut batch = self.batch();
        if let Some(props) = start_create {
            batch.insert_node(&stmt.label, &start_key, props);
            created += 1;
        }
        if let Some(props) = end_create {
            batch.insert_node(&edge.end_label, &end_key, props);
            created += 1;
        }
        let sets = if edge_existed {
            &match_sets
        } else {
            batch.insert_edge(&edge.etype, src_key, dst_key);
            &create_sets
        };
        for (key, field, value) in sets {
            batch.set_prop(key, field, resolve_merge_set_value(value, params)?);
        }
        batch.commit()?;

        // Refresh the role mask so created nodes are visible to this
        // statement's RETURN (read-after-write); see `exec_merge`.
        if created > 0 {
            if let Some(role) = self.pending_write_authz.as_ref().map(|a| a.role.clone()) {
                let new_mask = self.mask_for_role(&role)?;
                if let Some(a) = self.pending_write_authz.as_mut() {
                    a.mask = new_mask;
                }
            }
        }

        if let Some(returns) = stmt.returns {
            let node =
                |var: Option<&String>, default: &str, label: &str, field: &str, key: &str| {
                    NodePat {
                        var: Some(var.cloned().unwrap_or_else(|| default.to_string())),
                        label: Some(label.to_string()),
                        props: vec![(field.to_string(), Operand::Lit(Value::Str(key.to_string())))],
                    }
                };
            let q = Query {
                matches: vec![Pattern {
                    start: node(
                        stmt.var.as_ref(),
                        "_mn0",
                        &stmt.label,
                        &stmt.key_field,
                        &start_key,
                    ),
                    chain: vec![(
                        core_query::cypher::RelPat {
                            var: edge.rel_var.clone(),
                            etypes: vec![edge.etype.clone()],
                            dir: edge.dir,
                            hops: None,
                        },
                        node(
                            edge.end_var.as_ref(),
                            "_mn1",
                            &edge.end_label,
                            &edge.end_key_field,
                            &end_key,
                        ),
                    )],
                    shortest: false,
                    path_var: None,
                    continues_match: false,
                }],
                optional_clauses: vec![],
                where_expr: None,
                unwinds: vec![],
                post_unwind_where: None,
                stages: vec![],
                returns,
                distinct: false,
                order_by: vec![],
                skip: None,
                limit: None,
            };
            let ops = plan(&q).map_err(|e| GraphError::QueryError {
                detail: format!("plan: {e}"),
            })?;
            let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
            return (if let Some(ref mask) = mask_opt {
                execute(&self.view_masked(mask), &ops, &Params(params))
            } else {
                execute(&self.view(), &ops, &Params(params))
            })
            .map_err(|e| GraphError::QueryError {
                detail: format!("execute: {e}"),
            });
        }

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(created)),
            Some(Value::Int(0)),
            Some(Value::Int(0)),
        ]);
        Ok(rs)
    }

    fn exec_merge(
        &mut self,
        stmt: core_query::cypher::MergeStmt,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        // MERGE: check if a node with the given key already exists.
        if stmt.edge.is_some() {
            return self.exec_merge_edge(stmt, params);
        }
        let key = match &stmt.key_value {
            Value::Str(s) => s.clone(),
            _ => {
                return Err(GraphError::QueryError {
                    detail: format!(
                        "MERGE key value must be a string (got {:?})",
                        stmt.key_value
                    ),
                })
            }
        };

        if let Some(var) = stmt.var.as_deref() {
            for sc in stmt.on_create.iter().chain(&stmt.on_match) {
                if sc.var != var {
                    return Err(GraphError::QueryError {
                        detail: format!(
                            "SET variable '{}' does not match MERGE variable '{var}'",
                            sc.var
                        ),
                    });
                }
            }
        }

        // ── MERGE authz pre-check (when role-scoped) ─────────────────────────
        //
        // MERGE scope precondition: check create OR update scope for the
        // declared label BEFORE calling `has_node` (timing-oracle closure,
        // spec §6.2 "MERGE visibility oracle" item: hidden ≡ absent for
        // unscoped roles — the scope denial fires without touching the key store).
        //
        // Clone to avoid holding a borrow on `self.pending_write_authz` while
        // also calling `self.ids.get(key)`.
        let merge_existed: bool = self.merge_node_existed(&stmt.label, &key)?;

        let existed = merge_existed;
        let create_props = if existed {
            None
        } else {
            Some(self.merge_create_props(&stmt.key_field, &stmt.key_value, stmt.ns.as_ref())?)
        };
        let mut created = 0i64;
        if create_props.is_some() || !stmt.on_match.is_empty() {
            let mut batch = self.batch();
            if let Some(props) = create_props {
                batch.insert_node(&stmt.label, &key, props);
                for sc in &stmt.on_create {
                    let value = resolve_merge_set_value(&sc.value, params)?;
                    batch.set_prop(&key, &sc.field, value);
                }
                created = 1;
            } else {
                for sc in &stmt.on_match {
                    let value = resolve_merge_set_value(&sc.value, params)?;
                    batch.set_prop(&key, &sc.field, value);
                }
            }
            batch.commit()?;
        }

        // Refresh the role mask so the just-created node is visible to this
        // statement's RETURN (read-after-write). Safe: create_labels ⊆ read labels
        // (apply_schema subset rule), so the new node's label is already in the
        // role's read scope — this never widens beyond the role's declared labels.
        if !existed {
            if let Some(role) = self.pending_write_authz.as_ref().map(|a| a.role.clone()) {
                let new_mask = self.mask_for_role(&role)?;
                if let Some(a) = self.pending_write_authz.as_mut() {
                    a.mask = new_mask;
                }
            }
        }

        // Optional RETURN clause: project the node (created or matched) as a read result.
        if let Some(returns) = stmt.returns {
            let var = stmt.var.as_deref().unwrap_or("_mn0");
            let q = Query {
                matches: vec![Pattern {
                    start: NodePat {
                        var: Some(var.to_string()),
                        label: Some(stmt.label.clone()),
                        props: vec![("id".to_string(), Operand::Lit(stmt.key_value.clone()))],
                    },
                    chain: vec![],
                    shortest: false,
                    path_var: None,
                    continues_match: false,
                }],
                optional_clauses: vec![],
                where_expr: None,
                unwinds: vec![],
                post_unwind_where: None,
                stages: vec![],
                returns,
                distinct: false,
                order_by: vec![],
                skip: None,
                limit: None,
            };
            let ops = plan(&q).map_err(|e| GraphError::QueryError {
                detail: format!("plan: {e}"),
            })?;
            // Use view_masked when a role-scoped write is in flight so the
            // post-merge projection is consistent with the masked read phase.
            let mask_opt = self.pending_write_authz.as_ref().map(|a| a.mask.clone());
            return (if let Some(ref mask) = mask_opt {
                execute(&self.view_masked(mask), &ops, &Params(params))
            } else {
                execute(&self.view(), &ops, &Params(params))
            })
            .map_err(|e| GraphError::QueryError {
                detail: format!("execute: {e}"),
            });
        }

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(created)),
            Some(Value::Int(0)),
            Some(Value::Int(0)),
        ]);
        Ok(rs)
    }

    /// Return all rule-owned edges between `key_a` and `key_b` (either direction),
    /// annotated with rule name, edge type, direction, and weight.
    /// Results are sorted by (rule, edge_type).
    /// Returns `Err(KeyNotFound)` if either key is unknown.
    pub fn explain(&self, key_a: &str, key_b: &str) -> Result<Vec<Explanation>> {
        self.ensure_v8_base_sections_loaded();
        let id_a = self
            .ids
            .get(key_a)
            .ok_or_else(|| GraphError::KeyNotFound { key: key_a.into() })?;
        let id_b = self
            .ids
            .get(key_b)
            .ok_or_else(|| GraphError::KeyNotFound { key: key_b.into() })?;

        let mut results = Vec::new();

        // Walk the smaller incident set so explain is O(min(deg(a), deg(b)))
        // rather than O(total provenance).
        let scan = if self.engine.provenance_touching_len(id_a)
            <= self.engine.provenance_touching_len(id_b)
        {
            id_a
        } else {
            id_b
        };
        for (rule_name, etype, src, dst) in self.engine.provenance_touching(scan) {
            if !((src == id_a && dst == id_b) || (src == id_b && dst == id_a)) {
                continue;
            }
            let Some(rule_def) = self.engine.rules().find(|r| r.name == rule_name) else {
                continue;
            };
            let edge_type = match self.syms.resolve(etype) {
                Some(s) => s.to_string(),
                None => continue,
            };
            // Provenance (src, dst) ids come from the archived PROVENANCE section
            // (large, no eager CRC).  A corrupt section can produce ids that are
            // out of range; return Corrupt rather than panic.
            let src_key = self
                .ids
                .key_of(src)
                .ok_or_else(|| GraphError::Corrupt {
                    detail: format!("v8: provenance src id {src} not in id table"),
                })?
                .to_string();
            let dst_key = self
                .ids
                .key_of(dst)
                .ok_or_else(|| GraphError::Corrupt {
                    detail: format!("v8: provenance dst id {dst} not in id table"),
                })?
                .to_string();
            let stored = rule_def.weight_prop.as_deref().and_then(|prop| {
                self.edge_props_view()
                    .get(etype, src, dst, prop)
                    .and_then(|v| {
                        if let Value::Float(f) = v {
                            Some(f)
                        } else {
                            None
                        }
                    })
            });
            // Rules that store no weight (KeyMatch/FieldEqual defaults, auto-FK)
            // still have a score: recompute it from the predicate so explain
            // never reports "no score" for an edge the engine scored.  Via-hop
            // rules score over their via set, not over (src, dst), so leave
            // those None rather than report a number the rule did not produce.
            let weight = stored.or_else(|| {
                if rule_def.via_edge.is_some() {
                    return None;
                }
                let props_view = build_props_view(&self.props, &self.base);
                let src_get = |field: &str| props_view.get(src, field).map(|vr| vr.into_value());
                let dst_get = |field: &str| props_view.get(dst, field).map(|vr| vr.into_value());
                let src_view = NodeView {
                    key: &src_key,
                    props: &src_get,
                };
                let dst_view = NodeView {
                    key: &dst_key,
                    props: &dst_get,
                };
                evaluate(&rule_def.predicate, &src_view, &dst_view)
            });
            results.push(Explanation {
                rule: rule_name.to_string(),
                edge_type,
                src_key,
                dst_key,
                weight,
                predicate: PredicateSummary {
                    approximate: rule_def.approximate,
                    ..PredicateSummary::from(&rule_def.predicate)
                },
                via_edge: rule_def.via_edge.clone(),
            });
        }

        results.sort_by(|a, b| a.rule.cmp(&b.rule).then(a.edge_type.cmp(&b.edge_type)));
        Ok(results)
    }

    /// [`explain`](Self::explain) with the scoped read contract (§5.3): both
    /// endpoints are subject-checked, and any explanation whose evidence runs
    /// through a hidden node is **dropped entirely, not redacted**.
    ///
    /// A plain two-node rule's evidence is the pair itself, so once both
    /// subjects are visible there is nothing left to hide. A **via-hop** rule is
    /// different: it fires `src → dst` because some node carrying `via_label`
    /// sits between them, and [`Explanation`] carries the hop's edge *type*
    /// (`via_edge`) and never the hop's key. There is no field to blank, so a
    /// redacted explanation would still say "these two are linked through
    /// something you cannot see" — which discloses that the something exists.
    /// The explanation is therefore kept only when at least one **visible** via
    /// node satisfies the rule on its own.
    ///
    /// The weight is the **visible corpus's** number, not the store's: a via-hop
    /// rule stores the max over every via it hopped through, so the stored value
    /// can be a score only a hidden via produced. It is recomputed over the
    /// visible vias alone.
    ///
    /// Hidden or unknown `key_a` or `key_b` → [`GraphError::KeyNotFound`].
    pub fn explain_scoped(
        &self,
        key_a: &str,
        key_b: &str,
        mask: &crate::mask::NodeMask,
    ) -> Result<Vec<Explanation>> {
        for key in [key_a, key_b] {
            if !mask.contains_node(self, key) {
                return Err(GraphError::KeyNotFound { key: key.into() });
            }
        }
        Ok(self
            .explain(key_a, key_b)?
            .into_iter()
            .filter_map(|e| self.scoped_explanation(e, mask))
            .collect())
    }

    /// `e` as a caller limited to `mask` may have it, or `None` when it must be
    /// dropped entirely.
    ///
    /// Every non-via-hop explanation passes through untouched: its only nodes
    /// are the two subjects, which [`explain_scoped`](Self::explain_scoped) has
    /// already checked, and its weight is scored over that pair alone.
    ///
    /// A via-hop explanation is kept only when some via node the caller may see
    /// satisfies the rule on its own — and then its weight is recomputed as the
    /// max over exactly those vias. The engine writes the max over **all** of
    /// them (`core-rules::engine`, `best = prev.max(score)`), so passing the
    /// stored number through would let a hidden node set a figure the caller
    /// reads: the same disclosure dropping the explanation exists to prevent.
    ///
    /// A rule that stores no weight still reports none. The recomputed score is
    /// a sanitised version of a number `explain` already returned, never a new
    /// one — a scoped read must not say more than the unscoped read it narrows.
    fn scoped_explanation(
        &self,
        e: Explanation,
        mask: &crate::mask::NodeMask,
    ) -> Option<Explanation> {
        let Some(via_edge) = e.via_edge.clone() else {
            return Some(e);
        };
        let Some(rule_def) = self.engine.rules().find(|r| r.name == e.rule) else {
            // The rule is gone but its provenance is not; nothing can vouch for
            // the hop, so nothing is shown.
            return None;
        };
        let Some(via_label) = rule_def.via_label.as_deref() else {
            return Some(e);
        };
        let (Some(src), Some(dst)) = (self.ids.get(&e.src_key), self.ids.get(&e.dst_key)) else {
            return None;
        };
        let (Some(via_etype), Some(via_sym)) = (self.syms.get(&via_edge), self.syms.get(via_label))
        else {
            return None;
        };
        let via_dir = rule_def.via_dir.unwrap_or(Direction::Out);
        let props_view = build_props_view(&self.props, &self.base);
        let dst_get = |field: &str| props_view.get(dst, field).map(|vr| vr.into_value());
        let dst_view = NodeView {
            key: &e.dst_key,
            props: &dst_get,
        };
        // The rule's own namespace test, the one the engine applies to each via
        // candidate (`core-rules::engine::rule_sees_node`). Without it a
        // visible, out-of-namespace via — right label, satisfying predicate —
        // vouches for a hop the engine never made, and an explanation whose real
        // evidence is a hidden in-namespace node is kept.
        let rule_sees = |id: u32| match rule_def.namespace.as_deref() {
            None => true,
            Some(ns) => {
                let value = props_view.get(id, NS_PROP).map(|vr| vr.into_value());
                namespace_of_value(value.as_ref()) == ns
            }
        };
        let best = self
            .topo_view()
            .neighbors(via_etype, via_dir, src)
            .iter()
            .copied()
            .filter_map(|via| {
                if !mask.contains_id(via) {
                    return None;
                }
                if self.labels.get(via as usize).copied() != Some(via_sym) {
                    return None;
                }
                if !rule_sees(via) {
                    return None;
                }
                let via_key = self.ids.key_of(via)?;
                let via_get = |field: &str| props_view.get(via, field).map(|vr| vr.into_value());
                let via_view = NodeView {
                    key: via_key,
                    props: &via_get,
                };
                evaluate(&rule_def.predicate, &via_view, &dst_view)
            })
            .fold(None::<f64>, |best, score| {
                Some(match best {
                    None => score,
                    Some(prev) => prev.max(score),
                })
            })?;
        let weight = e.weight.map(|_| best);
        Some(Explanation { weight, ..e })
    }

    pub fn neighbors(&self, key: &str, edge_type: &str, dir: Direction) -> Result<Vec<String>> {
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
        let Some(sym) = self.syms.get(edge_type) else {
            return Ok(Vec::new());
        };
        self.topo_view()
            .neighbors(sym, dir, id)
            .iter()
            .map(|&n| {
                self.ids
                    .key_of(n)
                    .map(|k| k.to_string())
                    .ok_or_else(|| GraphError::Corrupt {
                        detail: format!("topology id {n} has no key"),
                    })
            })
            .collect::<Result<Vec<_>>>()
    }

    /// Unique directed degree of `key`. Unknown key → [`GraphError::KeyNotFound`].
    /// Unknown `edge_type` → 0. [`crate::algo::AlgoDir::Both`] is out + in (sum).
    pub fn degree(
        &self,
        key: &str,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
    ) -> Result<u64> {
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
        let topo = self.topo_view();
        Ok(Self::unique_directed_degree(
            &topo, &self.syms, id, edge_type, direction,
        ))
    }

    /// [`degree`](Self::degree) summing each pair's **insert count** instead of
    /// counting each pair once (§5.13).
    ///
    /// The unique degree asks how many neighbours there are; this asks how many
    /// times they were inserted. A pair with no recorded count contributes 1,
    /// so on a store that never called
    /// [`enable_multiplicity`](Self::enable_multiplicity) this returns exactly
    /// what [`degree`](Self::degree) returns rather than erroring — the
    /// distinction is a readout preference, not a demand the store cannot meet.
    ///
    /// `AlgoDir::Both` still sums out + in, so a pair visible on both sides
    /// still contributes twice: multiplicity changes what a pair is worth, never
    /// how a direction is counted.
    pub fn degree_multiplicity(
        &self,
        key: &str,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
    ) -> Result<u64> {
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
        Ok(Self::multiplicity_directed_degree(
            &self.topo_view(),
            &self.edge_props_view(),
            &self.syms,
            id,
            edge_type,
            direction,
            None,
        ))
    }

    /// [`degree_multiplicity`](Self::degree_multiplicity) under a scope.
    ///
    /// The sum covers **visible pairs only**. A hidden neighbour's inserts stay
    /// out of it for the reason
    /// [`degree_scoped`](Self::degree_scoped) documents, and more sharply: an
    /// unscoped multiplicity count discloses not only that a hidden neighbour
    /// exists but how often it was written.
    ///
    /// Hidden or unknown `key` → [`GraphError::KeyNotFound`].
    pub fn degree_scoped_multiplicity(
        &self,
        key: &str,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
        mask: &crate::mask::NodeMask,
    ) -> Result<u64> {
        let id = self
            .ids
            .get(key)
            .filter(|&id| mask.contains_id(id))
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
        Ok(Self::multiplicity_directed_degree(
            &self.topo_view(),
            &self.edge_props_view(),
            &self.syms,
            id,
            edge_type,
            direction,
            Some(mask),
        ))
    }

    /// [`degree`](Self::degree) counting **only neighbours the mask admits**.
    ///
    /// The filter is a correctness requirement, not an optimisation: an
    /// unfiltered count discloses the existence of a hidden neighbour to a
    /// caller who cannot see it, which is the same leak
    /// [`node_edges_scoped`](Self::node_edges_scoped) exists to prevent —
    /// reached by arithmetic instead of by name.
    ///
    /// Hidden or unknown `key` → [`GraphError::KeyNotFound`]. Unknown
    /// `edge_type` is still 0, as it is unscoped.
    pub fn degree_scoped(
        &self,
        key: &str,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
        mask: &crate::mask::NodeMask,
    ) -> Result<u64> {
        let id = self
            .ids
            .get(key)
            .filter(|&id| mask.contains_id(id))
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
        let topo = self.topo_view();
        Ok(Self::visible_directed_degree(
            &topo, &self.syms, id, edge_type, direction, mask,
        ))
    }

    /// Unique directed degree for a subset or a label scan.
    ///
    /// Unknown keys in `keys` are omitted (mask-like). `keys = Some(&[])` →
    /// empty `Ok(vec![])`. `limit` is applied after sorting degree desc, key
    /// asc, and only when `Some`. Invalid `where_` → `QueryError`.
    #[allow(clippy::too_many_arguments)]
    pub fn degrees(
        &self,
        keys: Option<&[String]>,
        label: Option<&str>,
        where_: Option<&PropPredicate>,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
        limit: Option<usize>,
    ) -> Result<Vec<(String, u64)>> {
        self.degrees_inner(
            keys, label, where_, edge_type, direction, limit, None, false,
        )
    }

    /// [`degrees`](Self::degrees) reporting each row's **insert-count** sum
    /// instead of its unique neighbour count, as
    /// [`degree_multiplicity`](Self::degree_multiplicity) does for one key.
    ///
    /// The sort is still degree descending, key ascending — over the counts this
    /// reading produces — and `limit` still applies after it.
    #[allow(clippy::too_many_arguments)]
    pub fn degrees_multiplicity(
        &self,
        keys: Option<&[String]>,
        label: Option<&str>,
        where_: Option<&PropPredicate>,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
        limit: Option<usize>,
    ) -> Result<Vec<(String, u64)>> {
        self.degrees_inner(keys, label, where_, edge_type, direction, limit, None, true)
    }

    /// [`degrees_scoped`](Self::degrees_scoped) reporting insert counts.
    ///
    /// Both filters apply: a hidden key stays out of the result, and every
    /// row's sum covers its **visible** pairs only.
    #[allow(clippy::too_many_arguments)]
    pub fn degrees_scoped_multiplicity(
        &self,
        keys: Option<&[String]>,
        label: Option<&str>,
        where_: Option<&PropPredicate>,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
        limit: Option<usize>,
        mask: &crate::mask::NodeMask,
    ) -> Result<Vec<(String, u64)>> {
        self.degrees_inner(
            keys,
            label,
            where_,
            edge_type,
            direction,
            limit,
            Some(mask),
            true,
        )
    }

    /// [`degrees`](Self::degrees) with the scope applied on both sides: a hidden
    /// key is omitted from the input — whether it arrived in `keys` or came out
    /// of the `label`/`where_` scan — and every row's count is the count of its
    /// **visible** neighbours, for the reason
    /// [`degree_scoped`](Self::degree_scoped) documents.
    ///
    /// Unlike `degree_scoped`, a hidden key here is not
    /// [`GraphError::KeyNotFound`]: `degrees` already drops unknown keys
    /// silently, so hidden and absent stay one answer by staying out of the
    /// result. `limit` still applies after the sort, and so counts visible rows.
    #[allow(clippy::too_many_arguments)]
    pub fn degrees_scoped(
        &self,
        keys: Option<&[String]>,
        label: Option<&str>,
        where_: Option<&PropPredicate>,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
        limit: Option<usize>,
        mask: &crate::mask::NodeMask,
    ) -> Result<Vec<(String, u64)>> {
        self.degrees_inner(
            keys,
            label,
            where_,
            edge_type,
            direction,
            limit,
            Some(mask),
            false,
        )
    }

    /// The body shared by [`degrees`](Self::degrees) and
    /// [`degrees_scoped`](Self::degrees_scoped). `mask = None` is the unscoped
    /// contract unchanged.
    #[allow(clippy::too_many_arguments)]
    fn degrees_inner(
        &self,
        keys: Option<&[String]>,
        label: Option<&str>,
        where_: Option<&PropPredicate>,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
        limit: Option<usize>,
        mask: Option<&crate::mask::NodeMask>,
        multiplicity: bool,
    ) -> Result<Vec<(String, u64)>> {
        if let Some(pred) = where_ {
            pred.validate_named("where")
                .map_err(|detail| GraphError::QueryError { detail })?;
        }
        if matches!(keys, Some(ks) if ks.is_empty()) {
            return Ok(Vec::new());
        }
        let view = self.view();
        let ids: Vec<u32> = match keys {
            Some(ks) => {
                let mut seen = HashSet::new();
                let mut out = Vec::new();
                for k in ks {
                    let Some(id) = view.ids.get(k) else {
                        continue;
                    };
                    if !seen.insert(id) {
                        continue;
                    }
                    if let Some(pred) = where_ {
                        let holds = match view.prop(id, &pred.field) {
                            None => pred.holds(None),
                            Some(vr) => pred.holds(Some(vr.as_value())),
                        };
                        if !holds {
                            continue;
                        }
                    }
                    out.push(id);
                }
                out
            }
            None => Self::vector_candidates(&view, label, where_),
        };
        let mut out: Vec<(String, u64)> = ids
            .into_iter()
            // A hidden candidate leaves as quietly as an unknown key does.
            .filter(|&id| mask.is_none_or(|m| m.contains_id(id)))
            .filter_map(|id| {
                let key = self.ids.key_of(id)?.to_string();
                let deg = match (multiplicity, mask) {
                    // The same `view` the unique arms read, so the per-row
                    // rebuild F9 measured is gone and all three arms agree on
                    // the state they are reading.
                    (true, m) => Self::multiplicity_directed_degree(
                        &view.topo,
                        &view.edge_props,
                        view.syms,
                        id,
                        edge_type,
                        direction,
                        m,
                    ),
                    (false, Some(m)) => Self::visible_directed_degree(
                        &view.topo, view.syms, id, edge_type, direction, m,
                    ),
                    (false, None) => Self::unique_directed_degree(
                        &view.topo, view.syms, id, edge_type, direction,
                    ),
                };
                Some((key, deg))
            })
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if let Some(lim) = limit {
            out.truncate(lim);
        }
        Ok(out)
    }

    /// Unique neighbour count for `id` across `edge_type` (or all types) and
    /// `direction`. Unknown `edge_type` → 0. `Both` sums out + in.
    fn unique_directed_degree(
        topo: &TopologyView<'_>,
        syms: &Interner,
        id: u32,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
    ) -> u64 {
        let dirs: &[Direction] = match direction {
            crate::algo::AlgoDir::Out => &[Direction::Out],
            crate::algo::AlgoDir::In => &[Direction::In],
            crate::algo::AlgoDir::Both => &[Direction::Out, Direction::In],
        };
        match edge_type {
            Some(name) => {
                let Some(et) = syms.get(name) else {
                    return 0;
                };
                dirs.iter().map(|&d| topo.degree(et, d, id) as u64).sum()
            }
            None => topo
                .etypes()
                .map(|et| {
                    dirs.iter()
                        .map(|&d| topo.degree(et, d, id) as u64)
                        .sum::<u64>()
                })
                .sum(),
        }
    }

    /// [`unique_directed_degree`](Self::unique_directed_degree) counting only
    /// neighbours `mask` admits.
    ///
    /// Same shape, one substitution: `topo.degree` is a length, so it cannot be
    /// filtered; the neighbour list it measures can. `Both` still sums out + in,
    /// so a node visible on both sides still counts twice — the filter changes
    /// which neighbours are counted, never how a degree is defined.
    fn visible_directed_degree(
        topo: &TopologyView<'_>,
        syms: &Interner,
        id: u32,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
        mask: &crate::mask::NodeMask,
    ) -> u64 {
        let dirs: &[Direction] = match direction {
            crate::algo::AlgoDir::Out => &[Direction::Out],
            crate::algo::AlgoDir::In => &[Direction::In],
            crate::algo::AlgoDir::Both => &[Direction::Out, Direction::In],
        };
        let visible = |et: u32| -> u64 {
            dirs.iter()
                .map(|&d| {
                    topo.neighbors(et, d, id)
                        .iter()
                        .filter(|&&n| mask.contains_id(n))
                        .count() as u64
                })
                .sum()
        };
        match edge_type {
            Some(name) => syms.get(name).map_or(0, visible),
            None => topo.etypes().map(visible).sum(),
        }
    }

    /// Sum of the insert counts of `id`'s pairs (§5.13), over `edge_type` (or
    /// all types) and `direction`, restricted to what `mask` admits when one is
    /// given.
    ///
    /// The same neighbour lists the unique reading measures, with each entry
    /// worth its pair's count rather than worth 1 — so the filter decides which
    /// pairs are in the sum and the count decides what each contributes. A
    /// direction decides which way round the pair is addressed: an `In`
    /// neighbour `n` of `id` is the pair `(et, n, id)`.
    ///
    /// Takes its views as parameters, exactly as the unique helpers do, because
    /// it is called once per row from a label scan. `edge_props_view()` reaches
    /// into the mmap'd base's rkyv section on every call, so building the two
    /// views inside made an N-row `degrees(multiplicity=True)` do N section
    /// accesses where the unique reading does one: worth 2.57 ms of 16.68 ms
    /// over 20 000 rows, about 0.13 us per row (defect #29,
    /// `tests/f9_bench.rs`). Most of that call's cost is the per-neighbour
    /// count lookup and is inherent, so this is a hoist, not a rescue.
    ///
    /// The views are exactly `self.view()`'s own `topo` and `edge_props`, so a
    /// caller that already has a view passes its halves and reads the same
    /// state it reads everything else from.
    fn multiplicity_directed_degree(
        topo: &TopologyView<'_>,
        edge_props: &EdgePropsView<'_>,
        syms: &Interner,
        id: u32,
        edge_type: Option<&str>,
        direction: crate::algo::AlgoDir,
        mask: Option<&crate::mask::NodeMask>,
    ) -> u64 {
        let dirs: &[Direction] = match direction {
            crate::algo::AlgoDir::Out => &[Direction::Out],
            crate::algo::AlgoDir::In => &[Direction::In],
            crate::algo::AlgoDir::Both => &[Direction::Out, Direction::In],
        };
        let count_of = |et: u32, src: u32, dst: u32| -> u64 {
            match edge_props.get(et, src, dst, EDGE_COUNT_PROP) {
                Some(Value::Int(n)) if n > 0 => n as u64,
                _ => 1,
            }
        };
        let per_etype = |et: u32| -> u64 {
            dirs.iter()
                .map(|&d| {
                    topo.neighbors(et, d, id)
                        .iter()
                        .filter(|&&n| mask.is_none_or(|m| m.contains_id(n)))
                        .map(|&n| match d {
                            Direction::Out => count_of(et, id, n),
                            Direction::In => count_of(et, n, id),
                        })
                        .sum::<u64>()
                })
                .sum()
        };
        match edge_type {
            Some(name) => syms.get(name).map_or(0, per_etype),
            None => topo.etypes().map(per_etype).sum(),
        }
    }

    /// Return the last-change commit sequence for `key`, or `None` if the node
    /// does not exist or has never been mutated since the last V5-V7 snapshot
    /// (horizon-bounded for legacy stores).
    ///
    /// The returned sequence is a monotonically increasing counter that starts
    /// at 1 for the first commit after `open` and increments with every
    /// successful write.  WAL replay at open also assigns sequences (1..N for N
    /// replayed frames), so sequences are consistent across snapshot+WAL cycles.
    ///
    /// For V5-V7 stores opened without a V8 snapshot, nodes that were present
    /// in the snapshot but not touched by any WAL frame will return `None`
    /// (horizon-bounded: CAS against such nodes is only safe after the first
    /// V8 snapshot or after the node is next mutated).
    pub fn last_changed(&self, key: &str) -> Option<u64> {
        let id = self.ids.get(key)?;
        self.last_change.get(&id).copied()
    }

    /// The `last_change` commit `key` had in the reader snapshot a transaction
    /// began from, worked out from this live handle.
    ///
    /// Used by [`crate::Transaction`] to build its conflict set lazily, when it
    /// first touches a key, without copying the whole `last_change` map at
    /// BEGIN. `base_version` and `base_store` are the snapshot's `commit_seq`
    /// and store identity; `in_base` says whether the key was live in it.
    ///
    /// The answer is exact whenever the key has not changed since the
    /// snapshot: no commit after `base_version` touched it, so its live entry
    /// is the snapshot's. When it has changed — a commit after `base_version`
    /// touched it, it was deleted, or it was inserted — the snapshot's value
    /// is no longer recoverable, and the answer is [`TX_CHANGED_SINCE_BEGIN`],
    /// which no live entry ever equals, so the commit fails as a conflict. A
    /// handle that reloaded from disk since the snapshot (another process
    /// snapshotted) renumbers nothing that persisted but can reseed
    /// `commit_seq`, so a reload answers "changed" for every key: conservative,
    /// never a missed conflict.
    pub(crate) fn tx_base_last_change(
        &self,
        key: &str,
        base_version: u64,
        base_store: crate::mask::StoreId,
        in_base: bool,
    ) -> u64 {
        if self.store_id != base_store {
            return TX_CHANGED_SINCE_BEGIN;
        }
        match self.ids.get(key) {
            // Absent now and at BEGIN: `NodeUnchangedSince { expected: 0 }`
            // conflicts with any insert that lands before the commit.
            None if !in_base => 0,
            // Deleted since BEGIN.
            None => TX_CHANGED_SINCE_BEGIN,
            // Inserted since BEGIN.
            Some(_) if !in_base => TX_CHANGED_SINCE_BEGIN,
            Some(id) => match self.last_change.get(&id).copied().unwrap_or(0) {
                seq if seq > base_version => TX_CHANGED_SINCE_BEGIN,
                seq => seq,
            },
        }
    }

    /// Which loaded store this handle is.
    ///
    /// Paired with [`commit_seq`](GraphDb::commit_seq) it identifies a graph
    /// state outright, which `commit_seq` alone does not: two stores of the same
    /// age share a sequence, and a reload can return to one. Memos of dense
    /// node ids that the handle does not own are stamped with both; see
    /// [`StoreStamp`](crate::mask::StoreStamp).
    pub(crate) fn store_id(&self) -> crate::mask::StoreId {
        self.store_id
    }

    /// The current commit sequence (number of successful commits since open,
    /// including WAL replay frames).  Useful for recording a baseline before
    /// a read-modify-write cycle.
    pub fn commit_seq(&self) -> u64 {
        self.commit_seq
    }

    /// Check that all `preconds` are satisfied against the current db state.
    /// Returns `Err(GraphError::CasConflict)` on the first failing precondition.
    pub(crate) fn check_preconditions(&self, preconds: &[Precondition]) -> Result<()> {
        for precond in preconds {
            match precond {
                Precondition::NodeUnchangedSince { key, expected } => {
                    // Missing entry means the node predates the WAL window or
                    // does not exist; treat as 0 (before any commit).
                    let actual = self.last_changed(key).unwrap_or_default();
                    if actual != *expected {
                        return Err(GraphError::CasConflict {
                            key: key.clone(),
                            expected: *expected,
                            actual,
                        });
                    }
                }
                Precondition::NodeAbsent { key } => {
                    // Node must not exist (not live).
                    if self.ids.get(key).is_some() {
                        let actual = self.last_changed(key).unwrap_or(0);
                        return Err(GraphError::CasConflict {
                            key: key.clone(),
                            expected: u64::MAX,
                            actual,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Apply a batch of mutations with compare-and-set preconditions.
    ///
    /// All preconditions are checked atomically before any operation is applied.
    /// If any precondition fails, the entire batch is rejected with
    /// [`GraphError::CasConflict`] and no WAL frame is written.
    ///
    /// # Returns
    /// `(nodes_inserted, edges_inserted)` on success, same as [`write_batch`].
    ///
    /// # Errors
    /// - [`GraphError::CasConflict`] if any precondition is not satisfied.
    /// - Any error that [`write_batch`] would return for the ops themselves.
    pub fn write_batch_cas(
        &mut self,
        preconds: Vec<Precondition>,
        ops: Vec<BatchOp>,
    ) -> Result<(usize, usize)> {
        self.check_preconditions(&preconds)?;
        self.commit_logged_batch(ops, None, None).map(inserted_pair)
    }

    /// Update the per-node last-change map for a WAL record at commit `seq`.
    ///
    /// Called after a successful apply to record which nodes were touched.
    /// For replay, called with the WAL-frame's replayed seq.
    ///
    /// Touch definition (see [`Precondition`] doc):
    /// - InsertNode / InsertNodeId / SetProp / SetPropId / RemoveProp → the node.
    /// - InsertEdge / InsertEdgeId / DeleteEdge → both src and dst.
    /// - DeleteNode → node tombstoned; last_changed() returns None so no update needed.
    /// - DerivedEdge markers, Intern, rule/view records → no-ops.
    /// - Batch → recurse into inner records.
    fn update_last_change_from_rec(&mut self, rec: &WalRecord, seq: u64) {
        match rec {
            WalRecord::InsertNode { key, .. }
            | WalRecord::SetProp { key, .. }
            | WalRecord::RemoveProp { key, .. } => {
                if let Some(id) = self.ids.get(key) {
                    self.last_change.insert(id, seq);
                }
            }
            WalRecord::InsertNodeId { key, .. } => {
                if let Some(id) = self.ids.get(key) {
                    self.last_change.insert(id, seq);
                }
            }
            WalRecord::SetPropId { id, .. } => {
                self.last_change.insert(*id, seq);
            }
            WalRecord::InsertEdge {
                src_key, dst_key, ..
            }
            | WalRecord::DeleteEdge {
                src_key, dst_key, ..
            } => {
                if let Some(src_id) = self.ids.get(src_key) {
                    self.last_change.insert(src_id, seq);
                }
                if let Some(dst_id) = self.ids.get(dst_key) {
                    self.last_change.insert(dst_id, seq);
                }
            }
            WalRecord::InsertEdgeId { src, dst, .. } => {
                self.last_change.insert(*src, seq);
                self.last_change.insert(*dst, seq);
            }
            // A count record touches the pair, so it touches both endpoints —
            // the same reading `InsertEdgeId` gets, because a duplicate insert
            // that raises the count *is* a mutation of that pair. The opt-in
            // declaration touches nothing.
            WalRecord::SetEdgeCount { src, dst, .. } if !rec.is_multiplicity_decl() => {
                self.last_change.insert(*src, seq);
                self.last_change.insert(*dst, seq);
            }
            WalRecord::SetEdgeCount { .. } => {}
            // DeleteNode: node is tombstoned; last_changed(key) returns None for
            // deleted keys (ids.get() returns None post-tombstone), so no update needed.
            // History markers: state no-ops; the underlying mutation already
            // touched the relevant nodes' last_change entries.
            WalRecord::DeleteNode { .. }
            | WalRecord::DerivedEdgeAdded { .. }
            | WalRecord::DerivedEdgeRetracted { .. }
            | WalRecord::Intern { .. }
            | WalRecord::CreateRule { .. }
            | WalRecord::DeleteRule { .. }
            | WalRecord::RebuildRule { .. }
            | WalRecord::CreateView { .. }
            | WalRecord::DeleteView { .. }
            | WalRecord::EnableFulltext { .. }
            | WalRecord::DisableFulltext { .. }
            | WalRecord::EnableIndex { .. }
            | WalRecord::DisableIndex { .. }
            | WalRecord::CreateConstraint { .. }
            | WalRecord::DropConstraint { .. }
            | WalRecord::EnableRangeIndex { .. }
            | WalRecord::DisableRangeIndex { .. } => {}
            // RenameNode: node id is stable; update last_change via the new key.
            // Called after apply(), so ids already reflects new_key.
            WalRecord::RenameNode { new_key, .. } => {
                if let Some(id) = self.ids.get(new_key) {
                    self.last_change.insert(id, seq);
                }
            }
            WalRecord::Batch(inner) => {
                for inner_rec in inner {
                    self.update_last_change_from_rec(inner_rec, seq);
                }
            }
        }
    }

    pub fn node_count(&self) -> usize {
        self.ids.len()
    }

    /// Configure archive retention: keep the `N` newest WAL archives at each
    /// [`snapshot_with`] call when `archive_wal: true`.
    ///
    /// `Some(N)` where N > 0 → prune oldest archives keeping the newest N.
    /// `Some(0)` or `None` → unlimited (no pruning).
    ///
    /// Pruning only ever happens inside [`snapshot_with`]; this method only
    /// stores the policy.  Archives below the retention limit are deleted
    /// oldest-first.  The horizon floor is updated so that
    /// [`was_linked`] / history APIs return `CommitOutOfRange` for commits
    /// in pruned archives rather than silently returning wrong data.
    pub fn set_wal_archive_retention(&mut self, keep: Option<u32>) {
        self.wal_archive_retention = keep;
    }

    /// Delete any WAL archives that are fully below the current horizon floor.
    ///
    /// Orphaned archives arise when the floor is written first during retention
    /// pruning and then a crash interrupts the archive-delete sequence.  The
    /// opening cleanup ensures no subsequent read path sees stale data.
    ///
    /// Under the monotonic naming scheme, the archive name N equals the
    /// cumulative end-frame index of the archive in global commit space (i.e.
    /// the archive covers global frames `[prev_n, N)`).  An archive is
    /// fully orphaned when `N <= wal_horizon_floor`: all of its frames fall
    /// below the floor and have already been counted in it.
    fn cleanup_orphaned_archives(&mut self) -> Result<()> {
        if self.wal_horizon_floor == 0 {
            // Floor at 0 means no pruning has ever occurred; nothing to clean.
            return Ok(());
        }
        let archive_ns = self.fs.list_archives()?;
        for n in archive_ns {
            if n <= self.wal_horizon_floor {
                // Archive N ends at global frame N; all its frames are below
                // the floor (floor already accounts for them) → orphaned.
                self.fs.delete_archive(n).map_err(GraphError::Io)?;
            } else {
                // Archives are sorted ascending; first one above floor stops scan.
                break;
            }
        }
        Ok(())
    }

    /// Collect all WAL frames from surviving archives (oldest-first) then the
    /// live WAL into one flat list, and return the total along with the number
    /// of archive frames at the front of the list.
    ///
    /// Commit indices into the returned list are LOCAL (0 = first frame of
    /// oldest surviving archive).  To obtain the GLOBAL index add
    /// `self.wal_horizon_floor`.
    /// How many frames the surviving archives hold, without materialising them.
    ///
    /// The same count `all_frames` puts at the front of its list. Used to seed
    /// [`wal_frames_written`](GraphDb::wal_frames_written) at open without
    /// decoding the live WAL a second time; free on a store with no archives,
    /// which is most of them.
    fn archive_frame_count(&self) -> Result<u64> {
        let mut n = 0u64;
        for a in self.fs.list_archives()? {
            let bytes = self.fs.read_archive(a)?;
            let (frames, _) = decode_all(&bytes);
            n += frames.len() as u64;
        }
        Ok(n)
    }

    fn all_frames(&self) -> Result<(Vec<WalRecord>, u64)> {
        let archive_ns = self.fs.list_archives()?;
        let mut all: Vec<WalRecord> = Vec::new();
        for n in archive_ns {
            let bytes = self.fs.read_archive(n)?;
            let (frames, _) = decode_all(&bytes);
            all.extend(frames);
        }
        let archive_count = all.len() as u64;
        let live_bytes = self.fs.read(FileId::Wal)?;
        let (live_frames, _) = decode_all(&live_bytes);
        all.extend(live_frames);
        Ok((all, archive_count))
    }

    /// Return the total number of committed WAL frames visible in the current
    /// horizon window, including frames in surviving WAL archives.
    ///
    /// This is the exclusive upper bound for valid `at_commit` indices in
    /// `was_linked`. Valid indices are `wal_horizon_floor()..wal_total_commits()`.
    ///
    /// Returns the horizon floor when all surviving history is empty.
    pub fn wal_total_commits(&self) -> Result<u64> {
        // The floor that belongs with these frames: both are read from disk
        // here. A handle that has not refreshed since another process moved
        // the floor would otherwise number new frames from its old one (row 81).
        let floor = self.fs.read_horizon_floor()?;
        let (frames, _) = self.all_frames()?;
        Ok(floor + frames.len() as u64)
    }

    /// The global frame index of the first commit reachable through surviving
    /// archives (0 when no archives have been pruned).
    pub fn wal_horizon_floor(&self) -> u64 {
        self.wal_horizon_floor
    }

    /// Return the per-node change history for `key` by scanning the on-disk WAL.
    ///
    /// ## Horizon
    ///
    /// History reaches back only to the last WAL-truncating snapshot, exactly like `open_at`.
    /// Snapshots written with `keep_wal: true` preserve deeper history. This is the honest,
    /// zero-cost contract; a durable history log is out of scope.
    ///
    /// ## Derived edges
    ///
    /// Rule-created (derived) edges are **not** in the WAL and therefore do not appear in
    /// history. Only edges written directly by the application are recorded.
    ///
    /// ## Deleted nodes
    ///
    /// For nodes that have been deleted, dense-id records (SetPropId, InsertEdgeId) that
    /// predate the deletion may not resolve (the id is tombstoned in the live map). The
    /// string-keyed `DeleteNode` record still matches and produces a `NodeDeleted` entry.
    /// Prop/edge history of a deleted node may therefore be partially unresolvable.
    ///
    /// ## Dense-id edge entries and tombstoned partners
    ///
    /// Edge entries from dense-id WAL records (`InsertEdgeId`) are omitted when the partner
    /// endpoint's dense id is tombstoned. As a result, a live node's history can contain an
    /// `EdgeRemoved` (string-keyed, always resolves) without a corresponding `EdgeAdded`.
    /// Build commit-bounded alias intervals for `queried_key`.
    ///
    /// Returns a list of `(key, valid_from_inclusive, valid_until_exclusive)` tuples.
    /// A record written under `key` at commit `c` matches the queried identity iff
    /// `c >= valid_from && (valid_until.is_none() || c < valid_until)`.
    ///
    /// Each alias entry carries both a lower and an upper bound so that key-reuse
    /// after a rename is handled correctly: if "a" is renamed to "b" at commit 5,
    /// then a NEW node is created as "a" at commit 7 and renamed to "c" at commit 10,
    /// querying "c" must NOT surface identity-1's events (commits 0–4 under "a");
    /// only identity-2's events (commits 7–9 under "a") are in scope.
    ///
    /// Only **forward aliasing**: querying the *new* key surfaces events written
    /// under the *old* key.  The reverse direction is not supported.
    fn build_key_alias_intervals(
        &self,
        frames: &[core_storage::wal::WalRecord],
        queried_key: &str,
        floor: u64,
    ) -> Vec<(String, u64, Option<u64>)> {
        self.key_alias_intervals(frames, queried_key, floor, false)
    }

    /// [`Self::build_key_alias_intervals`], and with `current_holder_only`
    /// the queried key itself is bounded too: it counts from the commit the
    /// node that has it now acquired it. An earlier node that held the same
    /// key and was deleted is then no part of the answer — neither its edges
    /// nor its deletion.
    fn key_alias_intervals(
        &self,
        frames: &[core_storage::wal::WalRecord],
        queried_key: &str,
        floor: u64,
        current_holder_only: bool,
    ) -> Vec<(String, u64, Option<u64>)> {
        use core_storage::wal::WalRecord;

        // Pre-pass: build reverse_rename and key_starts maps.
        // Every rename into a key, in commit order. A key can be renamed into
        // more than once — `a → b`, `b → a`, `a → b` — and which rename led
        // to the occupancy being followed depends on when.
        let mut reverse_rename: HashMap<String, Vec<(String, u64)>> = HashMap::new();
        let mut key_starts: HashMap<String, Vec<u64>> = HashMap::new();

        for (local_i, frame) in frames.iter().enumerate() {
            let commit = floor + local_i as u64;
            let records: &[WalRecord] = match frame {
                WalRecord::Batch(inner) => inner.as_slice(),
                single => std::slice::from_ref(single),
            };
            for rec in records {
                match rec {
                    WalRecord::InsertNode { key, .. } | WalRecord::InsertNodeId { key, .. } => {
                        key_starts.entry(key.clone()).or_default().push(commit);
                    }
                    WalRecord::RenameNode { old_key, new_key } => {
                        // new_key came into existence at this commit.
                        key_starts.entry(new_key.clone()).or_default().push(commit);
                        // Record the reverse rename: new_key was introduced by renaming old_key.
                        reverse_rename
                            .entry(new_key.clone())
                            .or_default()
                            .push((old_key.clone(), commit));
                    }
                    _ => {}
                }
            }
        }

        // Build alias intervals by following the reverse rename chain.
        let mut result: Vec<(String, u64, Option<u64>)> = Vec::new();
        let mut current_key = queried_key.to_string();
        let mut current_valid_until: Option<u64> = None;

        loop {
            // valid_from: the most recent commit where current_key was assigned to this
            // identity.  For aliases (valid_until = Some(vu)), find the last start event
            // for the key strictly before vu — this is where the alias's occupancy by
            // this identity began, correctly excluding prior identities that reused the key.
            let valid_from = if let Some(vu) = current_valid_until {
                key_starts
                    .get(&current_key)
                    .and_then(|starts| starts.iter().rev().find(|&&s| s < vu).copied())
                    .unwrap_or(floor)
            } else if current_holder_only {
                key_starts
                    .get(&current_key)
                    .and_then(|starts| starts.last().copied())
                    .unwrap_or(floor)
            } else {
                // Queried key — no upper bound; may have been introduced at any commit.
                floor
            };

            result.push((current_key.clone(), valid_from, current_valid_until));

            // The rename that led here: the latest into this key, and for an
            // alias the latest before the alias stopped being this node's.
            // Each step goes strictly back in time, so the walk ends — taking
            // the last rename whatever its commit sent `a → b → a` round for
            // ever.
            let led_here = reverse_rename
                .get(&current_key)
                .and_then(|renames| {
                    renames
                        .iter()
                        .rev()
                        .find(|(_, at)| current_valid_until.is_none_or(|until| *at < until))
                })
                // And only if that rename is how this occupancy of the key
                // began. A key renamed into, deleted and inserted fresh
                // starts a new node at the insert; the older rename led to
                // the node that was deleted.
                .filter(|(_, at)| {
                    (current_valid_until.is_none() && !current_holder_only) || *at == valid_from
                });
            match led_here {
                Some((old_key, rename_commit)) => {
                    current_valid_until = Some(*rename_commit);
                    current_key = old_key.clone();
                }
                None => break,
            }
        }

        result
    }

    /// Returns true if `record_key` matches any alias interval that covers `commit`.
    fn aliases_match(
        intervals: &[(String, u64, Option<u64>)],
        record_key: &str,
        commit: u64,
    ) -> bool {
        intervals
            .iter()
            .any(|(k, vf, vu)| k == record_key && commit >= *vf && vu.is_none_or(|u| commit < u))
    }

    /// Return the change history of node `key` by scanning the on-disk WAL.
    ///
    /// ## Horizon
    ///
    /// History reaches back only as far as the retained WAL. The returned
    /// [`HistoryResult`](crate::history::HistoryResult) carries `total_commits`
    /// (the exclusive upper bound for valid commit indices) and `horizon` (the
    /// oldest commit still reachable). When `horizon > 0`, older events were
    /// pruned and are not in `items`.
    pub fn node_history(
        &self,
        key: &str,
    ) -> Result<crate::history::HistoryResult<crate::history::HistoryEntry>> {
        use crate::history::{HistoryChange, HistoryEntry, HistoryResult};
        use core_storage::wal::WalRecord;

        // The floor that belongs with these frames: both are read from disk
        // here. A handle that has not refreshed since another process moved
        // the floor would otherwise number new frames from its old one (row 81).
        let floor = self.fs.read_horizon_floor()?;
        let (frames, _) = self.all_frames()?;
        let total_commits = floor + frames.len() as u64;

        // Resolve commit-bounded alias intervals for `key` (handles renames in the WAL).
        let alias_intervals = self.build_key_alias_intervals(&frames, key, floor);

        let mut out: Vec<HistoryEntry> = Vec::new();

        for (local_i, frame) in frames.iter().enumerate() {
            let commit = floor + local_i as u64;
            // Collect the inner records to process — Batch is one commit, single records are one commit.
            let records: &[WalRecord] = match frame {
                WalRecord::Batch(inner) => inner.as_slice(),
                single => std::slice::from_ref(single),
            };

            for rec in records {
                let change = match rec {
                    WalRecord::InsertNode { label, key: k, .. }
                        if Self::aliases_match(&alias_intervals, k, commit) =>
                    {
                        Some(HistoryChange::NodeInserted {
                            label: label.clone(),
                        })
                    }
                    WalRecord::InsertNodeId { label, key: k, .. }
                        if Self::aliases_match(&alias_intervals, k, commit) =>
                    {
                        let label_str = match self.syms.resolve(*label) {
                            Some(s) => s.to_string(),
                            None => continue,
                        };
                        Some(HistoryChange::NodeInserted { label: label_str })
                    }
                    WalRecord::SetProp {
                        key: k,
                        field,
                        value,
                    } if Self::aliases_match(&alias_intervals, k, commit) => {
                        Some(HistoryChange::PropSet {
                            field: field.clone(),
                            value: value.clone(),
                        })
                    }
                    WalRecord::SetPropId { id, field, value } => {
                        // Use key_of_historical (not key_of) so a node's prop_set
                        // events remain visible after the node is later deleted:
                        // key_of returns None for a tombstoned id, which would
                        // silently drop every PropSet between insert and delete.
                        // Mirrors the InsertEdgeId arm below and edge_history's
                        // own id-keyed arms.
                        match self.ids.key_of_historical(*id) {
                            // key_of_historical returns the last-known (possibly
                            // post-rename, possibly post-delete) key; compare to queried key.
                            Some(resolved) if resolved == key => {
                                let field_str = match self.syms.resolve(*field) {
                                    Some(s) => s.to_string(),
                                    None => continue,
                                };
                                Some(HistoryChange::PropSet {
                                    field: field_str,
                                    value: value.clone(),
                                })
                            }
                            _ => None,
                        }
                    }
                    WalRecord::RemoveProp { key: k, field }
                        if Self::aliases_match(&alias_intervals, k, commit) =>
                    {
                        Some(HistoryChange::PropRemoved {
                            field: field.clone(),
                        })
                    }
                    WalRecord::InsertEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if Self::aliases_match(&alias_intervals, src_key, commit) {
                            Some(HistoryChange::EdgeAdded {
                                edge_type: edge_type.clone(),
                                other: dst_key.clone(),
                                outgoing: true,
                            })
                        } else if Self::aliases_match(&alias_intervals, dst_key, commit) {
                            Some(HistoryChange::EdgeAdded {
                                edge_type: edge_type.clone(),
                                other: src_key.clone(),
                                outgoing: false,
                            })
                        } else {
                            None
                        }
                    }
                    WalRecord::InsertEdgeId { etype, src, dst } => {
                        let etype_str = match self.syms.resolve(*etype) {
                            Some(s) => s.to_string(),
                            None => continue,
                        };
                        // key_of_historical (not key_of): an edge added before
                        // either endpoint was later deleted must still resolve —
                        // see the SetPropId arm above and edge_history's
                        // InsertEdgeId arm, which use the same lookup for the
                        // same reason.
                        let src_key = self.ids.key_of_historical(*src);
                        let dst_key = self.ids.key_of_historical(*dst);
                        if src_key == Some(key) {
                            let other = match dst_key {
                                Some(s) => s.to_string(),
                                None => continue,
                            };
                            Some(HistoryChange::EdgeAdded {
                                edge_type: etype_str,
                                other,
                                outgoing: true,
                            })
                        } else if dst_key == Some(key) {
                            let other = match src_key {
                                Some(s) => s.to_string(),
                                None => continue,
                            };
                            Some(HistoryChange::EdgeAdded {
                                edge_type: etype_str,
                                other,
                                outgoing: false,
                            })
                        } else {
                            None
                        }
                    }
                    WalRecord::DeleteEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if Self::aliases_match(&alias_intervals, src_key, commit) {
                            Some(HistoryChange::EdgeRemoved {
                                edge_type: edge_type.clone(),
                                other: dst_key.clone(),
                                outgoing: true,
                            })
                        } else if Self::aliases_match(&alias_intervals, dst_key, commit) {
                            Some(HistoryChange::EdgeRemoved {
                                edge_type: edge_type.clone(),
                                other: src_key.clone(),
                                outgoing: false,
                            })
                        } else {
                            None
                        }
                    }
                    WalRecord::DeleteNode { key: k }
                        if Self::aliases_match(&alias_intervals, k, commit) =>
                    {
                        Some(HistoryChange::NodeDeleted)
                    }
                    // Skip: rule/view/fulltext/intern metadata; Batch wrapper handled above.
                    _ => None,
                };

                if let Some(change) = change {
                    out.push(HistoryEntry { commit, change });
                }
            }
        }

        Ok(HistoryResult {
            items: out,
            total_commits,
            horizon: floor,
        })
    }

    /// Return the per-edge change history between nodes `a` and `b` by scanning
    /// the on-disk WAL.
    ///
    /// ## Horizon
    ///
    /// History reaches back only to the last WAL-truncating snapshot, exactly
    /// like `node_history` and `open_at`. The returned [`HistoryResult`] carries
    /// `total_commits` (= number of WAL frames), which is the exclusive upper
    /// bound for valid commit indices.
    ///
    /// ## Derived edges
    ///
    /// Rule-derived edges appear via `DerivedEdgeAdded` / `DerivedEdgeRetracted`
    /// WAL markers written by `log_then_apply_with` after each rule-firing
    /// mutation. The `rule` field of those events carries the rule name.
    ///
    /// ## DeleteNode
    ///
    /// When a node is deleted, its manual incident edges are swept inline without
    /// individual `DeleteEdge` WAL records. `edge_history` detects `DeleteNode`
    /// events for either endpoint and synthesises `Retracted(rule:None)` events
    /// for each manual edge that was active at that point. Derived edges active at
    /// the time of deletion are handled by the `DerivedEdgeRetracted` marker that
    /// the engine appends immediately after the `DeleteNode` record; those events
    /// carry correct rule attribution and are emitted by the marker arm, not the
    /// synthetic sweep.
    ///
    /// ## Masks
    ///
    /// Like `node_history`, this method has no mask parameter and returns WAL
    /// history regardless of any role mask. For masked history semantics, apply
    /// the mask at the caller level.
    pub fn edge_history(
        &self,
        a: &str,
        b: &str,
    ) -> Result<crate::history::HistoryResult<crate::history::EdgeHistoryEvent>> {
        use crate::history::{EdgeEvent, EdgeHistoryEvent, HistoryResult};
        use core_storage::wal::WalRecord;

        // The floor that belongs with these frames: both are read from disk
        // here. A handle that has not refreshed since another process moved
        // the floor would otherwise number new frames from its old one (row 81).
        let floor = self.fs.read_horizon_floor()?;
        let (frames, _) = self.all_frames()?;
        let total_commits = floor + frames.len() as u64;

        // Resolve all historical names for a and b (handles RenameNode in the WAL).
        // Intervals are commit-bounded so recycled keys don't contaminate histories.
        let alias_a = self.build_key_alias_intervals(&frames, a, floor);
        let alias_b = self.build_key_alias_intervals(&frames, b, floor);

        // Active edges between a and b tracked as (edge_type, src_key, dst_key, is_derived).
        // The is_derived flag is used by the DeleteNode sweep: manual edges are
        // swept with a synthetic Retracted(rule:None); derived edges are skipped
        // because the engine writes a DerivedEdgeRetracted marker immediately after
        // the DeleteNode record, which carries the correct rule attribution.
        let mut active: Vec<(String, String, String, bool)> = Vec::new();
        let mut out: Vec<EdgeHistoryEvent> = Vec::new();

        for (local_i, frame) in frames.iter().enumerate() {
            let commit = floor + local_i as u64;
            let records: &[WalRecord] = match frame {
                WalRecord::Batch(inner) => inner.as_slice(),
                single => std::slice::from_ref(single),
            };

            for rec in records {
                match rec {
                    WalRecord::InsertEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        let is_ab = Self::aliases_match(&alias_a, src_key, commit)
                            && Self::aliases_match(&alias_b, dst_key, commit);
                        let is_ba = Self::aliases_match(&alias_b, src_key, commit)
                            && Self::aliases_match(&alias_a, dst_key, commit);
                        if is_ab || is_ba {
                            active.push((
                                edge_type.clone(),
                                src_key.clone(),
                                dst_key.clone(),
                                false,
                            ));
                            out.push(EdgeHistoryEvent {
                                edge_type: edge_type.clone(),
                                commit,
                                event: EdgeEvent::Added,
                                rule: None,
                            });
                        }
                    }
                    WalRecord::InsertEdgeId { etype, src, dst } => {
                        let etype_str = match self.syms.resolve(*etype) {
                            Some(s) => s.to_string(),
                            None => continue,
                        };
                        // Use key_of_historical so tombstoned nodes (deleted
                        // later in the WAL) still resolve during the scan.
                        let src_key = self.ids.key_of_historical(*src);
                        let dst_key = self.ids.key_of_historical(*dst);
                        let is_ab = src_key == Some(a) && dst_key == Some(b);
                        let is_ba = src_key == Some(b) && dst_key == Some(a);
                        if is_ab || is_ba {
                            let src_str = src_key.unwrap().to_string();
                            let dst_str = dst_key.unwrap().to_string();
                            active.push((etype_str.clone(), src_str, dst_str, false));
                            out.push(EdgeHistoryEvent {
                                edge_type: etype_str,
                                commit,
                                event: EdgeEvent::Added,
                                rule: None,
                            });
                        }
                    }
                    WalRecord::DeleteEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        let is_ab = Self::aliases_match(&alias_a, src_key, commit)
                            && Self::aliases_match(&alias_b, dst_key, commit);
                        let is_ba = Self::aliases_match(&alias_b, src_key, commit)
                            && Self::aliases_match(&alias_a, dst_key, commit);
                        if is_ab || is_ba {
                            // Remove the first matching active entry (flag ignored).
                            if let Some(pos) = active.iter().position(|(et, s, d, _)| {
                                et == edge_type && s == src_key && d == dst_key
                            }) {
                                active.remove(pos);
                            }
                            out.push(EdgeHistoryEvent {
                                edge_type: edge_type.clone(),
                                commit,
                                event: EdgeEvent::Retracted,
                                rule: None,
                            });
                        }
                    }
                    WalRecord::DeleteNode { key: k }
                        if Self::aliases_match(&alias_a, k, commit)
                            || Self::aliases_match(&alias_b, k, commit) =>
                    {
                        // Sweep: implicitly retract only MANUAL active edges.
                        // Derived active edges are skipped here because the rule
                        // engine appends a DerivedEdgeRetracted marker immediately
                        // after this DeleteNode record; that marker produces the
                        // single correctly-attributed Retracted event.  Derived
                        // entries are dropped from `active` (the marker arm's
                        // idempotent retain finds nothing to remove).
                        for (et, _, _, is_derived) in active.drain(..) {
                            if !is_derived {
                                out.push(EdgeHistoryEvent {
                                    edge_type: et,
                                    commit,
                                    event: EdgeEvent::Retracted,
                                    rule: None,
                                });
                            }
                            // Derived: drop silently; marker carries the Retracted event.
                        }
                    }
                    WalRecord::DerivedEdgeAdded {
                        rule,
                        edge_type: et,
                        src_key,
                        dst_key,
                    } => {
                        let is_ab = Self::aliases_match(&alias_a, src_key, commit)
                            && Self::aliases_match(&alias_b, dst_key, commit);
                        let is_ba = Self::aliases_match(&alias_b, src_key, commit)
                            && Self::aliases_match(&alias_a, dst_key, commit);
                        if is_ab || is_ba {
                            active.push((et.clone(), src_key.clone(), dst_key.clone(), true));
                            out.push(EdgeHistoryEvent {
                                edge_type: et.clone(),
                                commit,
                                event: EdgeEvent::Added,
                                rule: Some(rule.clone()),
                            });
                        }
                    }
                    WalRecord::DerivedEdgeRetracted {
                        rule,
                        edge_type: et,
                        src_key,
                        dst_key,
                    } => {
                        let is_ab = Self::aliases_match(&alias_a, src_key, commit)
                            && Self::aliases_match(&alias_b, dst_key, commit);
                        let is_ba = Self::aliases_match(&alias_b, src_key, commit)
                            && Self::aliases_match(&alias_a, dst_key, commit);
                        if is_ab || is_ba {
                            // Push unconditionally: a derived edge whose Added marker
                            // predates the history horizon has no `active` entry, but
                            // the retraction is still a real in-window event.
                            // Remove from active idempotently if present.
                            active.retain(|(aet, s, d, _)| {
                                !(aet == et && s == src_key && d == dst_key)
                            });
                            out.push(EdgeHistoryEvent {
                                edge_type: et.clone(),
                                commit,
                                event: EdgeEvent::Retracted,
                                rule: Some(rule.clone()),
                            });
                        }
                    }
                    // All other records (InsertNode, SetProp, CreateRule, etc.)
                    // do not affect edges between a and b.
                    _ => {}
                }
            }
        }

        Ok(HistoryResult {
            items: out,
            total_commits,
            horizon: floor,
        })
    }

    /// Return `true` iff an edge of `edge_type` existed between `a` and `b`
    /// (in either direction) at the WAL commit `at_commit`.
    ///
    /// ## Horizon
    ///
    /// Valid commit indices are `0..total_commits` where `total_commits` is the
    /// number of WAL frames. An `at_commit >= total_commits` is outside the
    /// visible horizon and returns [`GraphError::CommitOutOfRange`].
    ///
    /// ## Derived edges
    ///
    /// Rule-derived edges are tracked via `DerivedEdgeAdded` / `DerivedEdgeRetracted`
    /// WAL markers appended at firing time (Task 1). `was_linked` reads these markers
    /// and therefore includes derived edges in its point-in-time evaluation,
    /// matching `edge_history`'s fidelity.
    ///
    /// ## Which nodes
    ///
    /// `a` and `b` name the nodes that hold those keys now, and the answer
    /// follows each of them back through its renames. A node that held one of
    /// the keys earlier and was deleted is another node: for a commit at
    /// which that earlier node was linked, the answer is `false`.
    pub fn was_linked(&self, a: &str, b: &str, edge_type: &str, at_commit: u64) -> Result<bool> {
        use core_storage::wal::WalRecord;

        // The floor that belongs with these frames: both are read from disk
        // here. A handle that has not refreshed since another process moved
        // the floor would otherwise number new frames from its old one (row 81).
        let floor = self.fs.read_horizon_floor()?;
        let (frames, _) = self.all_frames()?;
        let total_commits = floor + frames.len() as u64;

        // Horizon floor: commits in pruned archives are unreachable.
        if at_commit < floor {
            return Err(GraphError::CommitOutOfRange {
                commit: at_commit,
                total: total_commits,
                floor,
            });
        }
        if at_commit >= total_commits {
            return Err(GraphError::CommitOutOfRange {
                commit: at_commit,
                total: total_commits,
                floor,
            });
        }

        // Resolve all historical names for a and b (handles RenameNode in the WAL).
        // Intervals are commit-bounded so recycled keys don't contaminate point-in-time reads.
        let alias_a = self.key_alias_intervals(&frames, a, floor, true);
        let alias_b = self.key_alias_intervals(&frames, b, floor, true);
        // The two nodes themselves, when both are live. A record that names
        // its endpoints by dense id is then matched by id: a retired id keeps
        // the key it last had, and matching on that string took an earlier,
        // deleted holder of the key for the node that has it now.
        let live_ids = self.ids.get(a).zip(self.ids.get(b));

        // Local index into surviving frames (0 = first frame of oldest archive).
        let local_commit = at_commit - floor;

        // Replay local frames 0..=local_commit, tracking active edges between
        // the two nodes: the edge type and whether it runs from `a` to `b`.
        // Tracked by which node, not by the key each record names it with: an
        // edge written while a node was called one thing and deleted while it
        // was called another is the same edge.
        let mut active: BTreeSet<(String, bool)> = BTreeSet::new();

        for (local_i, frame) in frames.iter().enumerate().take((local_commit + 1) as usize) {
            let commit = floor + local_i as u64;
            let records: &[WalRecord] = match frame {
                WalRecord::Batch(inner) => inner.as_slice(),
                single => std::slice::from_ref(single),
            };

            for rec in records {
                match rec {
                    WalRecord::InsertEdge {
                        edge_type: et,
                        src_key,
                        dst_key,
                    } => {
                        let is_ab = Self::aliases_match(&alias_a, src_key, commit)
                            && Self::aliases_match(&alias_b, dst_key, commit);
                        let is_ba = Self::aliases_match(&alias_b, src_key, commit)
                            && Self::aliases_match(&alias_a, dst_key, commit);
                        if is_ab {
                            active.insert((et.clone(), true));
                        }
                        if is_ba {
                            active.insert((et.clone(), false));
                        }
                    }
                    WalRecord::InsertEdgeId { etype, src, dst } => {
                        let etype_str = match self.syms.resolve(*etype) {
                            Some(s) => s.to_string(),
                            None => continue,
                        };
                        // Use key_of_historical so tombstoned nodes resolve.
                        let src_key = self.ids.key_of_historical(*src);
                        let dst_key = self.ids.key_of_historical(*dst);
                        let (is_ab, is_ba) = match live_ids {
                            Some((id_a, id_b)) => {
                                (*src == id_a && *dst == id_b, *src == id_b && *dst == id_a)
                            }
                            None => (
                                src_key == Some(a) && dst_key == Some(b),
                                src_key == Some(b) && dst_key == Some(a),
                            ),
                        };
                        if is_ab {
                            active.insert((etype_str.clone(), true));
                        }
                        if is_ba {
                            active.insert((etype_str, false));
                        }
                    }
                    WalRecord::DeleteEdge {
                        edge_type: et,
                        src_key,
                        dst_key,
                    } => {
                        let is_ab = Self::aliases_match(&alias_a, src_key, commit)
                            && Self::aliases_match(&alias_b, dst_key, commit);
                        let is_ba = Self::aliases_match(&alias_b, src_key, commit)
                            && Self::aliases_match(&alias_a, dst_key, commit);
                        if is_ab {
                            active.remove(&(et.clone(), true));
                        }
                        if is_ba {
                            active.remove(&(et.clone(), false));
                        }
                    }
                    WalRecord::DeleteNode { key: k }
                        if Self::aliases_match(&alias_a, k, commit)
                            || Self::aliases_match(&alias_b, k, commit) =>
                    {
                        // One of the two nodes is gone, and every edge
                        // between them with it.
                        active.clear();
                    }
                    WalRecord::DerivedEdgeAdded {
                        edge_type: et,
                        src_key,
                        dst_key,
                        ..
                    } => {
                        let is_ab = Self::aliases_match(&alias_a, src_key, commit)
                            && Self::aliases_match(&alias_b, dst_key, commit);
                        let is_ba = Self::aliases_match(&alias_b, src_key, commit)
                            && Self::aliases_match(&alias_a, dst_key, commit);
                        if is_ab {
                            active.insert((et.clone(), true));
                        }
                        if is_ba {
                            active.insert((et.clone(), false));
                        }
                    }
                    WalRecord::DerivedEdgeRetracted {
                        edge_type: et,
                        src_key,
                        dst_key,
                        ..
                    } => {
                        let is_ab = Self::aliases_match(&alias_a, src_key, commit)
                            && Self::aliases_match(&alias_b, dst_key, commit);
                        let is_ba = Self::aliases_match(&alias_b, src_key, commit)
                            && Self::aliases_match(&alias_a, dst_key, commit);
                        if is_ab {
                            active.remove(&(et.clone(), true));
                        }
                        if is_ba {
                            active.remove(&(et.clone(), false));
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(active.iter().any(|(et, _)| et == edge_type))
    }

    /// Every edge incident to `key` — either endpoint — that existed at WAL
    /// commit `commit`, from ONE scan of the WAL.
    ///
    /// This is the bulk form of [`was_linked`](GraphDb::was_linked): answering
    /// "what did K's relationships look like at commit C" with one call instead
    /// of one [`edge_history`](GraphDb::edge_history) per candidate partner.
    /// The two agree edge for edge.
    ///
    /// Results are sorted by `(edge_type, src_key, dst_key)`.
    ///
    /// ## Horizon
    ///
    /// Valid commit indices are `wal_horizon_floor()..wal_total_commits()`;
    /// anything outside is [`GraphError::CommitOutOfRange`], exactly like
    /// `was_linked`. An unknown key is not an error — it simply had no edges.
    ///
    /// ## Derived edges
    ///
    /// `DerivedEdgeAdded` / `DerivedEdgeRetracted` markers carry rule
    /// attribution, so a rule-owned edge comes back with `derived: true` and
    /// `rule: Some(name)`.
    ///
    /// ## Renames
    ///
    /// `key` is matched through the same commit-bounded alias intervals
    /// `edge_history` uses, so querying a node's *current* key surfaces edges
    /// written under an earlier name. Endpoint keys in the result are reported
    /// under the name the node carries today, so they can be fed straight back
    /// into `node_info`, `explain` or another `edges_at`.
    ///
    /// ## Masks
    ///
    /// Like `edge_history` and `node_history`, this reads the WAL regardless of
    /// any role mask. Apply masking at the caller level.
    pub fn edges_at(&self, key: &str, commit: u64) -> Result<Vec<EdgeAt>> {
        use core_storage::wal::WalRecord;

        // Floor and frames from the same read of the disk (row 81).
        let floor = self.fs.read_horizon_floor()?;
        let (frames, _) = self.all_frames()?;
        let total_commits = floor + frames.len() as u64;

        // Horizon floor: commits in pruned archives are unreachable.
        if commit < floor || commit >= total_commits {
            return Err(GraphError::CommitOutOfRange {
                commit,
                total: total_commits,
                floor,
            });
        }

        // Commit-bounded historical names of `key` (handles RenameNode).
        let alias = self.build_key_alias_intervals(&frames, key, floor);

        // Forward rename chain, for reporting endpoints under their current
        // names: old key → [(commit, new key)] in ascending commit order.
        // Built over the whole WAL, not just the prefix up to `commit`, because
        // a rename after `commit` still changes what the node is called today.
        let mut renames: HashMap<String, Vec<(u64, String)>> = HashMap::new();
        for (local_i, frame) in frames.iter().enumerate() {
            let c = floor + local_i as u64;
            let records: &[WalRecord] = match frame {
                WalRecord::Batch(inner) => inner.as_slice(),
                single => std::slice::from_ref(single),
            };
            for rec in records {
                if let WalRecord::RenameNode { old_key, new_key } = rec {
                    renames
                        .entry(old_key.clone())
                        .or_default()
                        .push((c, new_key.clone()));
                }
            }
        }

        // The name a node written as `k` at commit `from` carries today.
        // Follows the first rename at or after `from`, then keeps going. The
        // iteration cap bounds a rename cycle inside a single batch.
        let canon = |k: &str, from: u64| -> String {
            if renames.is_empty() {
                return k.to_string();
            }
            let mut cur = k.to_string();
            let mut at = from;
            for _ in 0..64 {
                match renames
                    .get(&cur)
                    .and_then(|v| v.iter().find(|(c, _)| *c >= at))
                {
                    Some((c, new)) => {
                        at = *c;
                        cur = new.clone();
                    }
                    None => break,
                }
            }
            cur
        };

        let local_commit = commit - floor;
        // (edge_type, src_key, dst_key) → (derived, rule)
        let mut active: BTreeMap<(String, String, String), (bool, Option<String>)> =
            BTreeMap::new();

        for (local_i, frame) in frames.iter().enumerate().take((local_commit + 1) as usize) {
            let c = floor + local_i as u64;
            let records: &[WalRecord] = match frame {
                WalRecord::Batch(inner) => inner.as_slice(),
                single => std::slice::from_ref(single),
            };

            for rec in records {
                match rec {
                    WalRecord::InsertEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if Self::aliases_match(&alias, src_key, c)
                            || Self::aliases_match(&alias, dst_key, c)
                        {
                            active.insert(
                                (edge_type.clone(), canon(src_key, c), canon(dst_key, c)),
                                (false, None),
                            );
                        }
                    }
                    WalRecord::InsertEdgeId { etype, src, dst } => {
                        let Some(etype_str) = self.syms.resolve(*etype) else {
                            continue;
                        };
                        // `key_of_historical` resolves tombstoned ids too, and
                        // already returns the node's current key — no rename
                        // canonicalisation needed on this arm.
                        let (Some(src_key), Some(dst_key)) = (
                            self.ids.key_of_historical(*src),
                            self.ids.key_of_historical(*dst),
                        ) else {
                            continue;
                        };
                        if src_key == key || dst_key == key {
                            active.insert(
                                (
                                    etype_str.to_string(),
                                    src_key.to_string(),
                                    dst_key.to_string(),
                                ),
                                (false, None),
                            );
                        }
                    }
                    WalRecord::DeleteEdge {
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if Self::aliases_match(&alias, src_key, c)
                            || Self::aliases_match(&alias, dst_key, c)
                        {
                            active.remove(&(
                                edge_type.clone(),
                                canon(src_key, c),
                                canon(dst_key, c),
                            ));
                        }
                    }
                    WalRecord::DeleteNode { key: k } => {
                        if active.is_empty() {
                            continue;
                        }
                        if Self::aliases_match(&alias, k, c) {
                            // Our node is gone; every incident edge goes with it.
                            active.clear();
                        } else {
                            // A partner is gone; its edges to us go with it.
                            let ck = canon(k, c);
                            active.retain(|(_, s, d), _| *s != ck && *d != ck);
                        }
                    }
                    WalRecord::DerivedEdgeAdded {
                        rule,
                        edge_type,
                        src_key,
                        dst_key,
                    } => {
                        if Self::aliases_match(&alias, src_key, c)
                            || Self::aliases_match(&alias, dst_key, c)
                        {
                            active.insert(
                                (edge_type.clone(), canon(src_key, c), canon(dst_key, c)),
                                (true, Some(rule.clone())),
                            );
                        }
                    }
                    WalRecord::DerivedEdgeRetracted {
                        edge_type,
                        src_key,
                        dst_key,
                        ..
                    } => {
                        if Self::aliases_match(&alias, src_key, c)
                            || Self::aliases_match(&alias, dst_key, c)
                        {
                            active.remove(&(
                                edge_type.clone(),
                                canon(src_key, c),
                                canon(dst_key, c),
                            ));
                        }
                    }
                    // InsertNode, SetProp, CreateRule, … do not move edges.
                    _ => {}
                }
            }
        }

        // BTreeMap iteration is already (edge_type, src, dst) order.
        Ok(active
            .into_iter()
            .map(|((edge_type, src_key, dst_key), (derived, rule))| EdgeAt {
                edge_type,
                src_key,
                dst_key,
                derived,
                rule,
            })
            .collect())
    }

    /// The derived edges that would be retracted and derived if `key.field`
    /// were set to `value` — computed WITHOUT writing anything.
    ///
    /// Nothing is committed and nothing on `self` is mutated: the rule engine's
    /// provenance, its candidate indexes, the topology and the property columns
    /// are all cloned first, the change is applied to the clone, and the real
    /// per-node re-derivation (`RuleEngine::on_node_changed` — the same call
    /// `set_prop` makes during apply) runs against it. The derived-edge deltas
    /// it emits are the answer, so rule semantics — predicates, top-k,
    /// via-hops, chaining, weights — are the engine's, not a re-implementation.
    ///
    /// Works on a read-only handle.
    ///
    /// **While a rule's vector index is still building** (`RuleStats::building`)
    /// the clone carries no pending-build state, so this reports the edges that
    /// rule would derive — which the live store will not derive until its
    /// backfill runs. Right about the end state, early about the timing.
    ///
    /// Returns `Err(KeyNotFound)` for an unknown or tombstoned key and
    /// `Err(ViewPropReadOnly)` for a field a view owns — matching
    /// [`set_prop`](GraphDb::set_prop)'s validation. A change with no effect
    /// (the node already holds `value`, or no rule watches `field`) returns
    /// empty lists.
    ///
    /// ## Cost
    ///
    /// One clone of the property columns, the topology overlay, the symbol
    /// interner, the edge properties and the provenance map, plus one candidate
    /// re-index (O(nodes × rules)). That is much cheaper than copying the store
    /// directory, but it is not free — this is an interactive "what if", not a
    /// hot path.
    pub fn what_if_set_prop(&self, key: &str, field: &str, value: Value) -> Result<WhatIf> {
        // The engine's provenance, HNSW and IVF state live in the mmap'd base
        // until something asks for them. On a store opened cold from a snapshot
        // this is the first ask, and without it the clone below starts from an
        // empty provenance map: nothing to retract, so `lost` comes back empty.
        self.ensure_v8_base_sections_loaded();

        let empty = WhatIf {
            lost: Vec::new(),
            gained: Vec::new(),
        };

        if let Some(view_name) = self.view_store.view_for_prop(field) {
            return Err(GraphError::ViewPropReadOnly {
                view_name: view_name.to_string(),
            });
        }
        MutPreview::new(self).check_live_key(key)?;
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;

        let rules: Vec<RuleDef> = self.engine.rules().cloned().collect();
        if rules.is_empty() {
            return Ok(empty);
        }

        // No rule watches this field → no derivation can change.
        if !rules.iter().any(|r| r.watched_fields().contains(field)) {
            return Ok(empty);
        }

        let old_value = build_props_view(&self.props, &self.base)
            .get(id, field)
            .map(|vr| vr.into_value());
        if old_value.as_ref() == Some(&value) {
            return Ok(empty);
        }

        // --- Clone every piece of state the re-derivation writes to. ---
        // `what_if` mutates its own throwaway copies and writes nothing, so it
        // takes owned clones rather than sharing the `Arc`s. Deref-clone, not
        // `Arc::clone`: sharing here would make `make_mut` copy on first touch
        // anyway, and an owned local keeps the rest of this function unchanged.
        let mut props = (*self.props).clone();
        let mut topo = (*self.topo).clone();
        let mut syms = (*self.syms).clone();
        let mut edge_props = (*self.edge_props).clone();

        let mut tripped: BTreeMap<String, bool> = BTreeMap::new();
        let mut fires: BTreeMap<String, u64> = BTreeMap::new();
        for r in &rules {
            tripped.insert(r.name.clone(), self.engine.is_tripped(&r.name));
            fires.insert(r.name.clone(), self.engine.fire_count(&r.name));
        }
        // `provenance()` decodes retained snapshot bytes on first use; the
        // engine clone needs the real map, not an empty one.
        let provenance = self.engine.provenance().clone();
        let mut engine = core_rules::RuleEngine::from_persist(rules, provenance, tripped, fires);

        // Build the candidate indexes from the state BEFORE the change, exactly
        // as apply() sees them: `on_node_changed` withdraws the node under its
        // old value and refiles it under the new one, so the index must not
        // already reflect the change.
        engine.reindex_all_load_state(
            &self.ids,
            &syms,
            &self.labels,
            build_props_view(&self.props, &self.base),
            self.engine.export_ivf_state(),
            self.engine.export_hnsw_state_passthrough(),
        );
        engine.set_emit_deltas(true);

        // --- Apply the hypothetical change and re-derive. ---
        props.set(id, field, value);
        {
            let mut gm = make_graph_mut(
                &self.ids,
                &mut syms,
                &self.labels,
                build_props_view(&props, &self.base),
                &mut topo,
                &self.base,
                &mut edge_props,
            );
            engine.on_node_changed(id, Some((field, old_value)), &mut gm);
        }

        let mut lost: BTreeSet<EdgeAt> = BTreeSet::new();
        let mut gained: BTreeSet<EdgeAt> = BTreeSet::new();
        for d in engine.drain_deltas() {
            let edge = EdgeAt {
                edge_type: d.edge_type,
                src_key: d.src_key,
                dst_key: d.dst_key,
                derived: true,
                rule: Some(d.rule),
            };
            if d.fired {
                gained.insert(edge);
            } else {
                lost.insert(edge);
            }
        }
        // An edge retracted and re-derived within the same re-derivation (top-k
        // churn) is not a change the caller would see.
        let churn: Vec<EdgeAt> = lost.intersection(&gained).cloned().collect();
        for e in churn {
            lost.remove(&e);
            gained.remove(&e);
        }

        Ok(WhatIf {
            lost: lost.into_iter().collect(),
            gained: gained.into_iter().collect(),
        })
    }

    pub fn edge_count(&self) -> u64 {
        self.corrected_edge_count()
    }

    /// Total edge count, using the base CSR's actual row lengths when available
    /// (rather than the snapshot header's `edge_count` field, which is wrong on
    /// stores snapshotted while two rules owned one edge — ledger row 107).
    fn corrected_edge_count(&self) -> u64 {
        match self.base_edge_count_from_rows {
            // V8 base open: use the count we walked from the CSR rows at open.
            // Subtract tombstones (overlay deletions of base edges) and add the
            // overlay's own edges, exactly as TopologyView::edge_count does but
            // with the correct base count rather than the snapshot header's.
            Some(base_rows) => {
                let tombstones = self.topo.tombstone_count();
                base_rows.saturating_sub(tombstones) + self.topo.edge_count()
            }
            // No V8 base (WAL-only store or not yet refreshed): fall back to the
            // full topology view, which uses the snapshot header's count.  The
            // header is correct on stores that were never double-owned, so this
            // path is safe for every store that has not gone through row-107 damage.
            None => self.topo_view().edge_count(),
        }
    }

    /// Re-walk the current base CSR's out-rows to refresh `base_edge_count_from_rows`.
    ///
    /// Called after every snapshot write so the cached count stays aligned with the
    /// new base.  Without this, `corrected_edge_count` would carry the count from
    /// the previous open while the overlay (reset to empty by `snapshot_with`) is
    /// no longer the full topology.
    fn refresh_base_edge_count(&mut self) {
        self.base_edge_count_from_rows =
            self.base
                .as_ref()
                .and_then(|b| b.topology().ok())
                .map(|csr| {
                    csr.etypes
                        .iter()
                        .map(|e| {
                            e.out_adj
                                .rows
                                .iter()
                                .map(|r| r.neighbors.len() as u64)
                                .sum::<u64>()
                        })
                        .sum::<u64>()
                });
    }

    /// Live/tombstone/edge counts plus per-rule provenance size, trip latch,
    /// and fire counter (includes rebuild evaluations). Rules are sorted by name.
    pub fn stats(&self) -> Stats {
        self.ensure_v8_base_sections_loaded();
        let building = self.engine.builds_in_progress();
        let rules: Vec<RuleStats> = self
            .engine
            .rules()
            .map(|r| RuleStats {
                name: r.name.clone(),
                edges: self
                    .engine
                    .provenance()
                    .get(&r.name)
                    .map(|s| s.len() as u64)
                    .unwrap_or(0),
                tripped: self.engine.is_tripped(&r.name),
                fires: self.engine.fire_count(&r.name),
                approximate: r.approximate,
                building: building.iter().find(|b| b.rule == r.name).cloned(),
            })
            .collect();
        Stats {
            nodes_live: self.ids.live_len(),
            nodes_tombstoned: self.ids.len() - self.ids.live_len(),
            edges: self.corrected_edge_count(),
            rules,
            chain_truncations: self.engine.chain_truncations(),
            history_floor: self.wal_horizon_floor,
            namespaces: self.namespace_stats(),
        }
    }

    /// On-disk size of the WAL file in bytes.
    ///
    /// Reads file metadata without loading WAL contents.  Returns `Err` for
    /// in-memory (`SimFs`) databases where no WAL file exists on disk.
    pub fn wal_size_bytes(&self) -> std::io::Result<u64> {
        let path = self.fs.wal_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "wal_path not available for this Fs implementation",
            )
        })?;
        Ok(std::fs::metadata(path)?.len())
    }

    /// Set the slow-query threshold.  Queries whose execution time equals or
    /// exceeds `ms` milliseconds are logged.  Pass `0` to disable.
    ///
    /// Use this setter in tests — the environment variable
    /// `MUSHROOMDB_SLOW_QUERY_MS` is process-global and races parallel test
    /// threads.
    pub fn set_slow_query_threshold_ms(&mut self, ms: u64) {
        self.slow_query_threshold_ms = ms;
    }

    /// Snapshot of the slow-query ring buffer and lifetime counter.
    pub fn slow_query_snapshot(&self) -> SlowQuerySnapshot {
        let log = self.slow_queries.lock().unwrap_or_else(|e| e.into_inner());
        SlowQuerySnapshot {
            threshold_ms: self.slow_query_threshold_ms,
            count: log.total,
            last: log.entries.iter().cloned().collect(),
        }
    }

    /// Instant the database was opened.  Used by consumers (e.g. `/metrics`)
    /// to compute uptime.
    pub fn started_at(&self) -> std::time::Instant {
        self.started_at
    }

    /// The on-disk snapshot version a store that has opted in to nothing
    /// writes — the **floor**, not the whole answer.
    ///
    /// It is not "the version this binary writes", and it is not "the version
    /// this binary reads". This binary writes 9, 10 or 11 depending on the
    /// store — [`snapshot::version_for`] decides: a store that has called
    /// [`enable_multiplicity`](Self::enable_multiplicity) writes 10, and one
    /// that is [V11](Self::is_v11) writes 11 — and it reads 5 through 11. A caller comparing a store's
    /// stamp against this value must use `>=`, not `==`, or it will report an
    /// opted-in store as needing a migration *down*; `cli::run_migrate` is the
    /// worked example.
    ///
    /// The name is kept for compatibility: it is public API reachable from the
    /// CLI and from any embedder, and respelling it would break them for a
    /// doc-level clarification.
    ///
    /// [`snapshot::version_for`]: core_storage::snapshot::version_for
    pub fn format_version() -> u16 {
        core_storage::snapshot::VERSION
    }

    /// Test-support: total bytes appended (SimFs only usage).
    pub fn fs_total_appended(&self) -> usize
    where
        F: FsIntrospect,
    {
        self.fs.total_appended()
    }

    /// Test-support: successful `Fs::sync` calls (SimFs / counting fs).
    pub fn fs_sync_count(&self) -> usize
    where
        F: FsIntrospect,
    {
        self.fs.sync_count()
    }

    /// Consume the db, returning its fs (for crash simulation).
    pub fn into_fs(self) -> F {
        self.fs
    }

    pub fn snapshot(&mut self) -> Result<()> {
        self.snapshot_with(SnapshotOptions::default())
    }

    /// Snapshot with explicit options.
    ///
    /// # `keep_wal`
    ///
    /// When `keep_wal` is `false` (the default, same as [`snapshot`]):
    ///   - The WAL is replaced with a minimal baseline containing one
    ///     `EnableFulltext` record per active declaration.  All pre-snapshot
    ///     history is discarded; `open_at` can only reach post-snapshot commits.
    ///
    /// When `keep_wal` is `true`:
    ///   - The WAL is left intact.  All pre-snapshot commits remain reachable
    ///     via `open_at`.  The existing WAL already contains the original
    ///     `EnableFulltext` records, so no baseline re-write is needed; the
    ///     recovery guards in `apply()` silently skip any duplicate records on
    ///     replay.
    ///   - Crash window: a crash after the snapshot write but before the next
    ///     WAL write leaves the full pre-snapshot WAL intact.  On reopen the
    ///     snapshot is loaded and the WAL replayed idempotently over it — safe
    ///     because every `apply()` arm is idempotent when replayed over an
    ///     already-current snapshot.
    pub fn snapshot_with(&mut self, opts: SnapshotOptions) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        // A snapshot rewrites `wal.bin` through a tmp+rename, so a peer that is
        // appending ends up holding a descriptor on an unlinked inode and loses
        // commits it believes durable. Snapshotting therefore requires the
        // cross-process write lock, exactly as appending does. Unlike the WAL
        // append path this does not go through `log_then_apply_with`, so both
        // guards are repeated here.
        if self.degraded {
            return Err(GraphError::Io(std::io::Error::other(
                "database degraded after group-commit fsync failure; reopen required",
            )));
        }
        if self.lock_denied {
            return Err(self.lock_denied_error());
        }
        // A V11 store keeps its genesis marker under a name 0.9.x does not
        // read, before any snapshot can archive a WAL that holds V11 records
        // (see `Fs::use_v11_genesis_marker`).
        if self.v11_features {
            self.fs.use_v11_genesis_marker()?;
        }
        // From here `snapshot.bin` and `wal.bin` may be between states on disk.
        // A reader outside the engine pairing the two (the backup worker) waits
        // until the mark drops, on every exit from this function.
        let _snapshot_mark = self.durable_wal.begin_snapshot();
        let generation_at_start = self.durable_wal.get().generation;
        // Capture whether snapshot.bin already existed BEFORE this snapshot write.
        // Used by the archive path's conservative genesis-chain check: if a prior
        // snapshot exists but wal.truncated does not, we cannot distinguish a
        // legacy store (may have been truncated in an older code version) from a
        // new store that only used keep_wal=true.  Conservative: refuse genesis in
        // both cases.  Must be sampled here, before the snapshot write below.
        //
        // `snapshot_preserved_history` is the one case where the answer is not a
        // guess: a snapshot *this handle* took, on a store that had none when it
        // opened, and that kept the WAL. The proxy defers to it, because
        // otherwise `enable_multiplicity` — whose forced snapshot is exactly
        // that — would permanently disqualify the store from a genesis chain it
        // is fully entitled to (defect #23).
        let had_prior_snapshot = self.fs.snapshot_path().map(|p| p.exists()).unwrap_or(false)
            && !self.snapshot_preserved_history;
        // The genesis marker, for the first archive of a store whose history
        // has never been cut — written now, before anything is replaced.
        //
        // It says that this store's archives and WAL together reach back to
        // its first write, and that is as true now, with no archive yet, as
        // it will be when this snapshot has made one. It used to be written
        // last, after the snapshot, the rename and the new WAL, and a
        // snapshot interrupted anywhere before that never got another
        // chance: the next one found the interrupted one's `snapshot.bin`,
        // could not tell it from a truncating snapshot's, and declined. Every
        // commit in an archive was `out of range` from then on (row 98).
        //
        // Both conditions as before. No archive exists, and no snapshot did
        // before this one: a truncating snapshot always leaves `snapshot.bin`,
        // so one found here means the WAL may not start at the beginning, and
        // the chain is not claimed (never silently wrong data). The exception
        // is a snapshot this handle took itself with the WAL kept, which
        // `snapshot_preserved_history` records (defect #23). On SimFs
        // (`snapshot_path() == None`) there is never a prior snapshot.
        //
        // A marker left by a snapshot that then fails is still true. What
        // makes it false removes it: a truncating snapshot, and a retention
        // sweep that prunes an archive.
        if opts.archive_wal
            && !had_prior_snapshot
            && !self.archive_genesis_chain
            && self.fs.list_archives()?.is_empty()
        {
            self.fs.write_genesis_marker()?;
            self.archive_genesis_chain = true;
        }
        // Which version this store writes. V9 unless it has opted in to
        // something: V10 for multiplicity — the stamp that makes a reader which
        // does not know WAL discriminant 23 refuse the open instead of
        // truncating the WAL at the first such frame — and V11 for any V11
        // feature. V9 and V10 share every byte but these two header bytes; V11
        // adds section 18 and may hold the 0.10 value types in its columns.
        let snapshot_version =
            core_storage::snapshot::version_for(core_storage::snapshot::StoreFeatures {
                multiplicity: self.multiplicity,
                has_v11_types: self.v11_features,
                has_constraints: !self.constraints.is_empty(),
                has_range_indexes: self.prop_index.has_range_indexes(),
            });
        let v11 = snapshot_version == core_storage::snapshot::VERSION_11;
        // A snapshot that leaves the WAL where it is records how much of it
        // it holds, so the next open does not apply those writes again on top
        // of the state they produced. This handle holds the write lock, so
        // every byte in the WAL now is a write it has applied.
        let replaces_wal = opts.archive_wal || !opts.keep_wal;
        let wal_now = self.fs.read(FileId::Wal)?;
        let wal_mark = if replaces_wal {
            Vec::new()
        } else {
            core_storage::v8::WalMark::of(&wal_now).encode()
        };
        let standing_intent = if replaces_wal {
            self.standing_intent(&wal_now)?.0
        } else {
            None
        };
        let stop = self.snapshot_stop.take();
        let stopped = |at: SnapshotStop| -> Result<()> {
            if stop == Some(at) {
                return Err(GraphError::Io(std::io::Error::other(format!(
                    "snapshot stopped {at:?} (test hook)"
                ))));
            }
            Ok(())
        };
        self.ensure_v8_base_sections_loaded();
        // Ensure provenance is decoded before to_persist() clones it.
        self.engine.ensure_provenance_loaded_mut();
        let (rule_defs_typed, provenance, rule_tripped, rule_fires) = self.engine.to_persist();
        let rule_defs = rule_defs_typed
            .iter()
            .map(|r| bincode::serialize(r).expect("RuleDef serialize cannot fail"))
            .collect();
        // Collect HNSW state and IVF state.  When indexes are not yet
        // populated (clean open, no mutation since open), pass the retained
        // raw bytes through directly so that migrate/snapshot does not
        // silently discard fitted approximate-rule indexes.
        let hnsw_state = self.engine.export_hnsw_state_passthrough();
        let ivf_bytes = if !self.engine.indexes_populated() {
            // Pass retained IVF bytes through unchanged (no re-encode).
            self.engine.retained_ivf_bytes_clone().unwrap_or_default()
        } else {
            // Indexes live: encode from current state.
            let raw_ivf = self.engine.export_ivf_state();
            let ivf_state_map: BTreeMap<String, core_storage::snapshot::PerRuleIvfState> = raw_ivf
                .into_iter()
                .map(|(name, ((sc, sa, sd), (dc, da, dd)))| {
                    (
                        name,
                        core_storage::snapshot::PerRuleIvfState {
                            src: core_storage::snapshot::SideIvfState {
                                centroids: sc,
                                clusters: sa,
                                drift: sd,
                            },
                            dst: core_storage::snapshot::SideIvfState {
                                centroids: dc,
                                clusters: da,
                                drift: dd,
                            },
                        },
                    )
                })
                .collect();
            if ivf_state_map.is_empty() {
                Vec::new()
            } else {
                bincode::serialize(&ivf_state_map).expect("IVF state serialize cannot fail")
            }
        };
        let view_defs: Vec<Vec<u8>> = self
            .view_store
            .views()
            .map(|v| bincode::serialize(v).expect("ViewDef serialize cannot fail"))
            .collect();
        if self.base.is_some() {
            // V8 merge-snapshot path: encode base+overlay into a new V8 snapshot,
            // write it atomically, remap it as the new base, then clear the overlay.
            let meta = V8Meta {
                labels: (*self.labels).clone(),
                edge_props: (*self.edge_props).clone(),
                rule_defs,
                provenance,
                rule_tripped,
                rule_fires,
                ivf_bytes,
                view_defs,
                wal_truncated: !opts.keep_wal,
                hnsw: hnsw_state,
                last_change: self.last_change.clone(),
                fulltext_blob: self.fulltext_blob(),
                wal_mark: wal_mark.clone(),
                prov_reconciled: self.engine.lost_provenance_reconciled(),
                v11,
                declarations: self.snapshot_declarations(),
            };
            let mut buf: Vec<u8> = Vec::new();
            {
                // Clone the Arc so the old base stays alive while we encode.
                // The borrow of archived_csr (into old_base's mmap) is released
                // at the end of this block, before we replace self.base.
                let old_base = self.base.clone().expect("is_some checked above");
                let archived_csr = old_base.topology().map_err(|e| GraphError::Corrupt {
                    detail: format!("v8 snapshot: topology section: {e:?}"),
                })?;
                let archived_cols = old_base.columns().map_err(|e| GraphError::Corrupt {
                    detail: format!("v8 snapshot: columns section: {e:?}"),
                })?;
                // `None` when the base predates V9 — the migration path: its
                // string columns still carry their own tables and this snapshot
                // is the rewrite that collapses them into section 12.
                let archived_strings =
                    old_base
                        .string_table()
                        .transpose()
                        .map_err(|e| GraphError::Corrupt {
                            detail: format!("v8 snapshot: strings section: {e:?}"),
                        })?;
                let archived_edge_props =
                    old_base
                        .edge_props_section()
                        .map_err(|e| GraphError::Corrupt {
                            detail: format!("v8 snapshot: edge_props section: {e:?}"),
                        })?;
                let edge_props_raw =
                    old_base
                        .edge_props_raw_bytes()
                        .map_err(|e| GraphError::Corrupt {
                            detail: format!("v8 snapshot: edge_props raw bytes: {e:?}"),
                        })?;
                encode_v8(
                    Some(archived_csr),
                    Some(archived_cols),
                    archived_strings,
                    Some((archived_edge_props, edge_props_raw)),
                    // Never the old snapshot's provenance bytes. The encoder
                    // passes them through when the live map is empty, taking
                    // empty for "never loaded" — but it is loaded above, so
                    // empty means every rule's edges are gone, and passing
                    // the old section through brought them back as owned by
                    // rules that no longer derive them (ledger row 95).
                    None,
                    &self.topo,
                    &self.props,
                    &self.ids,
                    &self.syms,
                    &meta,
                    &mut buf,
                )?;
            }
            core_storage::snapshot::stamp_container_version(&mut buf, snapshot_version)?;
            if replaces_wal {
                // Before the snapshot: what is about to be written, and the
                // WAL it holds. From here until the WAL is replaced a crash
                // leaves that WAL beside the new snapshot, and this is how
                // the next open knows not to apply it again (ledger row 92).
                //
                // An intent already standing — the last snapshot was
                // interrupted too, and this store opens because of it — is
                // kept beside the new one until the new snapshot is down. If
                // this attempt fails before then, it is still what names the
                // snapshot on disk.
                let mut intent = core_storage::v8::SnapshotIntent::new(&buf, &wal_now).encode();
                if let Some(standing) = &standing_intent {
                    intent.extend_from_slice(&standing.encode());
                }
                self.fs.write_atomic(FileId::SnapshotIntent, &intent)?;
                // Until this snapshot empties it. If it does not get that far
                // and this handle goes on writing, the next append looks at
                // whether the intent is still true.
                self.intent_unsettled = true;
            }
            stopped(SnapshotStop::BeforeSnapshotWrite)?;
            self.fs.write_atomic(FileId::Snapshot, &buf)?;
            // Remap the freshly-written snapshot as the new base.
            // C2: use file mmap on RealFs; fall back to from_bytes on SimFs.
            let new_base = if let Some(snap_path) = self.fs.snapshot_path() {
                core_storage::v8::MappedBase::map(&snap_path)
            } else {
                core_storage::v8::MappedBase::from_bytes(buf)
            }
            .map_err(|e| GraphError::Corrupt {
                detail: format!("v8 snapshot: remap new base: {e:?}"),
            })?;
            self.base = Some(Arc::new(new_base));
            // Clear the overlay and prop tombstones — all data is now in the new base.
            self.topo = Arc::new(Topology::new());
            self.props = Arc::new(core_storage::columns::ColumnStore::new());
            // Keep the cached edge count aligned with the new base so that
            // corrected_edge_count() is correct for the rest of this session.
            self.refresh_base_edge_count();
        } else {
            // Legacy path (V5–V7 stores without a V8 base).
            //
            // Memory-diet path: build V8Meta directly from &self — no SnapshotState
            // clone and no encode_v8_from_state intermediate clones.  The big
            // structures (self.topo, self.props) are borrowed, not cloned.
            // self.edge_props is moved (not cloned) because we immediately clear it
            // when we remap the new V8 snapshot as self.base (see below).
            //
            // Eliminates from peak RSS vs. the old SnapshotState path:
            //   • self.topo.clone()      (~topology HashMap footprint)
            //   • self.props.clone()     (~column-store footprint)
            //   • encode_v8_from_state V8Meta secondary clones (labels, edge_props, …)
            let mut meta = V8Meta {
                labels: (*self.labels).clone(),
                wal_truncated: !opts.keep_wal,
                // Moved, not cloned, for the encode — and moved straight back
                // below, before anything that can fail. They are released
                // only once the new snapshot is mapped and reads go through
                // it. Left out of the handle across the write, a snapshot
                // that failed there went on with no edge properties at all,
                // and the next one that worked wrote that down.
                edge_props: std::mem::take(Arc::make_mut(&mut self.edge_props)),
                rule_defs,
                provenance,
                rule_tripped,
                rule_fires,
                ivf_bytes,
                view_defs,
                hnsw: hnsw_state,
                last_change: self.last_change.clone(),
                fulltext_blob: self.fulltext_blob(),
                wal_mark: wal_mark.clone(),
                prov_reconciled: self.engine.lost_provenance_reconciled(),
                v11,
                declarations: self.snapshot_declarations(),
            };
            let mut buf = Vec::new();
            let encoded = encode_v8(
                None,
                None,
                None,
                None,
                None,
                &self.topo,
                &self.props,
                &self.ids,
                &self.syms,
                &meta,
                &mut buf,
            );
            self.edge_props = Arc::new(std::mem::take(&mut meta.edge_props));
            drop(meta);
            encoded?;
            core_storage::snapshot::stamp_container_version(&mut buf, snapshot_version)?;
            if replaces_wal {
                // Before the snapshot: what is about to be written, and the
                // WAL it holds. From here until the WAL is replaced a crash
                // leaves that WAL beside the new snapshot, and this is how
                // the next open knows not to apply it again (ledger row 92).
                //
                // An intent already standing — the last snapshot was
                // interrupted too, and this store opens because of it — is
                // kept beside the new one until the new snapshot is down. If
                // this attempt fails before then, it is still what names the
                // snapshot on disk.
                let mut intent = core_storage::v8::SnapshotIntent::new(&buf, &wal_now).encode();
                if let Some(standing) = &standing_intent {
                    intent.extend_from_slice(&standing.encode());
                }
                self.fs.write_atomic(FileId::SnapshotIntent, &intent)?;
                // Until this snapshot empties it. If it does not get that far
                // and this handle goes on writing, the next append looks at
                // whether the intent is still true.
                self.intent_unsettled = true;
            }
            stopped(SnapshotStop::BeforeSnapshotWrite)?;
            self.fs.write_atomic(FileId::Snapshot, &buf)?;
            // Remap the freshly-written V8 snapshot as self.base.
            // On RealFs: drop the encode buffer before mmap to recover ~1.9 GiB.
            // On SimFs (tests): pass buf to from_bytes.
            let new_base = if let Some(snap_path) = self.fs.snapshot_path() {
                drop(buf);
                core_storage::v8::MappedBase::map(&snap_path)
            } else {
                core_storage::v8::MappedBase::from_bytes(buf)
            }
            .map_err(|e| GraphError::Corrupt {
                detail: format!("v8 snapshot: remap new base (legacy path): {e:?}"),
            })?;
            self.base = Some(Arc::new(new_base));
            // Free the large heap-allocated decoded state — all data is now in the
            // mmap'd base.  Mirrors the V8 merge-snapshot path (see above).
            self.edge_props = Arc::new(Default::default());
            self.topo = Arc::new(Topology::new());
            self.props = Arc::new(core_storage::columns::ColumnStore::new());
            // Keep the cached edge count aligned with the new base (same as the
            // V8 merge path above).
            self.refresh_base_edge_count();
        }

        stopped(SnapshotStop::AfterSnapshotWrite)?;
        if opts.archive_wal {
            // History-preserving snapshot (Task 4):
            //   1. Snapshot already written above (write_atomic → fsynced).
            //   2. Rename WAL → wal.<commit_seq>.archive  (atomic, same fs).
            //      Crash window B: crash here leaves archive present, WAL
            //      absent.  Reopen: snapshot loaded (full state), no WAL
            //      replay.  Archive is NOT replayed into live state — it is
            //      pre-snapshot by construction.  Safe.
            //   3. Write new minimal baseline WAL (write_atomic), with no
            //      fallible call between it and the rename.
            //      Crash window C: a crash between the two leaves the new
            //      archive and no live WAL.  Same as window B — handled above.
            //   4. Prune old archives (retention), update horizon floor.
            //      Pruning invalidates the genesis chain; delete marker.
            //   (The genesis marker is written before step 1; see above.
            //   Steps 2 and 3 are skipped when the WAL holds nothing new.)
            //
            // Sample existing archives BEFORE the rename: the last one names
            // where this one starts.
            let existing_archives = self.fs.list_archives()?;

            // Compute a globally-monotonic archive name: the name equals the
            // cumulative end-frame index of the archive in global commit space.
            //
            // Using `commit_seq` directly is UNSOUND across sessions: on reopen
            // commit_seq is seeded from max(last_change), which underestimates
            // the WAL depth when trailing commits (e.g. insert_edge) do not
            // update last_change.  A session-2 archive could then receive a name
            // ≤ the session-1 archive, causing incorrect sort order or collision.
            //
            // Instead: read and decode the live WAL here (before the rename) to
            // get its exact frame count, then add it to the last known global
            // end-frame index (the name of the most recent existing archive, or
            // wal_horizon_floor if no archives exist).  This is O(WAL size) but
            // snapshot is already serialising the full graph state, so the cost
            // is dominated.
            let live_wal_bytes_for_name = self.fs.read(FileId::Wal)?;
            let (live_frames_for_name, _) = decode_all(&live_wal_bytes_for_name);
            let archive_n = existing_archives
                .last()
                .copied()
                .unwrap_or(self.wal_horizon_floor)
                + live_frames_for_name.len() as u64;
            // What the WAL is replaced with: the store's declarations and
            // nothing else. Built before the rename, because it is also what
            // says whether there is anything to archive.
            let mut baseline_wal: Vec<u8> = Vec::new();
            // The multiplicity opt-in is a declaration like the two below it,
            // and it is re-emitted for the same reason: truncation must not
            // silently opt the store back out and stop counting.
            if self.multiplicity {
                baseline_wal
                    .extend_from_slice(&encode_record(&core_storage::wal::MULTIPLICITY_ENABLED));
            }
            for (label, field) in self.fulltext.enabled_pairs() {
                let rec = WalRecord::EnableFulltext {
                    label: label.clone(),
                    field: field.clone(),
                };
                baseline_wal.extend_from_slice(&encode_record(&rec));
            }
            for (label, field) in self.prop_index.equality_pairs() {
                let rec = WalRecord::EnableIndex {
                    label: label.clone(),
                    field: field.clone(),
                };
                baseline_wal.extend_from_slice(&encode_record(&rec));
            }
            // Only a store that holds `EnableRangeIndex` records has any, so a
            // store without them gets no V11 record here.
            for (name, (label, field)) in self.prop_index.range_indexes() {
                let rec = WalRecord::EnableRangeIndex {
                    name: name.clone(),
                    label: label.clone(),
                    field: field.clone(),
                };
                baseline_wal.extend_from_slice(&encode_record(&rec));
            }
            // After the indexes, so each constraint finds its backing index
            // already declared. Only a store that holds `CreateConstraint`
            // records has any, so a store without them gets no V11 record here.
            for c in &self.constraints {
                let rec = WalRecord::CreateConstraint {
                    name: c.name.clone(),
                    label: c.label.clone(),
                    field: c.field.clone(),
                };
                baseline_wal.extend_from_slice(&encode_record(&rec));
            }
            // A WAL that already holds exactly that has had nothing written
            // to it since the last snapshot put it there, and there is
            // nothing to archive. Archiving it anyway destroyed history: an
            // archive is named for the commit it ends at, so with no commit
            // since the last one this one took the last one's name, and the
            // rename put an empty file over it — after which the commit
            // numbers were handed out again and a read by commit answered
            // with a later state (row 100). The WAL stays where it is; it is
            // rewritten only if it ends in bytes that are no record. A store
            // with no archive yet has no name to collide with and is archived
            // as before.
            let (baseline_frames, _) = decode_all(&baseline_wal);
            //
            // An empty WAL has nothing to archive whatever the replacement
            // holds: it is what a kill between the rename and the write of
            // the replacement leaves, and on a store with a declaration it is
            // not the replacement. The rewrite below puts the declarations
            // back.
            //
            // A WAL with no bytes at all has nothing to archive even with no
            // archive yet: a store that has never committed has no `wal.bin`,
            // and renaming it failed with "No such file" after the snapshot
            // intent was written (ledger row 119).
            let nothing_to_archive = live_wal_bytes_for_name.is_empty()
                || ((live_frames_for_name.is_empty() || live_frames_for_name == baseline_frames)
                    && !self.fs.list_archives()?.is_empty());
            stopped(SnapshotStop::BeforeWalReplacement)?;
            if nothing_to_archive {
                if live_wal_bytes_for_name != baseline_wal {
                    self.durable_wal.replaced();
                    self.fs.write_atomic(FileId::Wal, &baseline_wal)?;
                }
            } else {
                // And never over an archive that exists, whatever led to the
                // name: a snapshot that cannot be taken costs a longer open,
                // an archive written over costs the store's history.
                if existing_archives.contains(&archive_n) {
                    return Err(GraphError::Corrupt {
                        detail: format!(
                            "snapshot: wal.{archive_n}.archive already exists; \
                             not archiving the WAL over it"
                        ),
                    });
                }
                self.durable_wal.replaced();
                self.fs.archive_wal(archive_n)?;

                // The replacement WAL goes in **immediately**, with no fallible call
                // between it and the rename above.
                //
                // The rename is what removes the store's live declarations — the
                // multiplicity opt-in, and every `EnableFulltext` / `EnableIndex` —
                // and this write is what puts them back. Every call that used to sit
                // in between (the genesis marker, the retention sweep's reads, the
                // floor write, the archive deletes) was a `?` that could leave the
                // store with neither, so a single transient `Err` was enough to lose
                // a declaration that no rebuild can recover (defect #22).
                //
                // Ordering alone cannot close the crash window between two
                // filesystem calls; for the multiplicity declaration the V10 stamp
                // does that on the open path. What ordering does close is the much
                // wider window in which an ordinary I/O error did it — and that half
                // covers all three declarations, not just the one with a stamp.
                self.fs.write_atomic(FileId::Wal, &baseline_wal)?;
            }

            // The genesis marker was written before the snapshot, above.

            // Retention pruning: keep newest `keep` archives; delete oldest.
            // Pruning is the ONLY deletion site for archives.
            //
            // Crash-safety ordering (C1 fix):
            //   1. Count frames in surplus archives (reads only — no mutation).
            //   2. Advance and PERSIST the horizon floor FIRST via write-then-
            //      rename (atomic).  A crash after this point leaves orphaned
            //      archives on disk, but the floor is correct.  The opening
            //      cleanup sweep (`cleanup_orphaned_archives`) removes them on
            //      the next open, so the store is always safe to reopen.
            //   3. Delete the genesis marker (floor > 0 already blocks open_at
            //      via the conjunctive gate; marker cleanup is belt-and-suspenders).
            //   4. Delete surplus archives.  A crash between any two deletes
            //      leaves the floor committed and orphaned archives cleaned at
            //      next open — never a stale floor with a missing archive prefix.
            if let Some(keep) = self.wal_archive_retention {
                if keep > 0 {
                    let archives = self.fs.list_archives()?;
                    // archives is sorted ascending (oldest first)
                    if archives.len() as u32 > keep {
                        let surplus = archives.len() - keep as usize;
                        // Step 1: count pruned frames (reads, no mutation).
                        let mut pruned_frames = 0u64;
                        for &n in &archives[..surplus] {
                            let bytes = self.fs.read_archive(n)?;
                            let (frames, _) = decode_all(&bytes);
                            pruned_frames += frames.len() as u64;
                        }
                        // Step 2: advance and persist floor FIRST.
                        self.wal_horizon_floor += pruned_frames;
                        self.fs.write_horizon_floor(self.wal_horizon_floor)?;
                        // The time map must not outlive the commits it
                        // describes: an entry below the new floor would resolve
                        // a date to a commit the engine can no longer replay,
                        // which is worse than having no entry at all.
                        self.commit_times.truncate_below(self.wal_horizon_floor);
                        self.rewrite_commit_times();
                        // Step 3: delete genesis marker (floor > 0 already
                        // blocks open_at; this is belt-and-suspenders cleanup).
                        if pruned_frames > 0 && self.archive_genesis_chain {
                            self.fs.delete_genesis_marker()?;
                            self.archive_genesis_chain = false;
                        }
                        // Step 4: delete surplus archives.  Crash here →
                        // orphaned archives; cleaned at next open.
                        for &n in &archives[..surplus] {
                            self.fs.delete_archive(n)?;
                        }
                    }
                }
            }
        } else if opts.keep_wal {
            // keep_wal=true: WAL is left untouched.  The existing WAL already
            // contains the EnableFulltext records from the original enable calls;
            // replay is idempotent (guards in apply() skip already-live entries).
            // No baseline re-write is needed or safe here — the full WAL history
            // must remain intact for open_at to reach pre-snapshot commits.
        } else {
            // keep_wal=false (default): truncate by replacing the WAL with a
            // minimal baseline of one EnableFulltext record per active pair.
            //
            // Crash-ordering: write_atomic is atomic.
            //   • Crash before snapshot write  → WAL unchanged.  Safe.
            //   • Crash after snapshot write but before this WAL write → full
            //     pre-snapshot WAL still present; open_with replays idempotently.
            //   • Crash after both writes → normal post-snapshot state.
            //
            // Genesis chain: a WAL-truncating snapshot breaks the archive chain
            // for any archives taken AFTER this point (their WAL slices would
            // not start at genesis).  Delete any existing genesis marker so that
            // open_at refuses archive-resident commits.  Future sessions are
            // covered by had_prior_snapshot: snapshot.bin written here persists
            // across sessions and prevents a later archiving session from
            // incorrectly claiming a complete genesis chain.
            if self.archive_genesis_chain {
                self.fs.delete_genesis_marker()?;
                self.archive_genesis_chain = false;
            }
            // And this handle can no longer prove the WAL is whole: it is about
            // to truncate it itself. Same-session archives after this point get
            // the conservative answer, exactly as cross-session ones do.
            self.snapshot_preserved_history = false;
            // The frames this discards keep their numbers. Everything the store
            // could still address — its archives and the live WAL — ends at
            // `wal_total_commits()`. That becomes the floor, and the replacement
            // WAL continues from it, exactly as the archive path's does after a
            // retention sweep.
            //
            // Leaving the floor where it was renumbered the new WAL over the
            // old one: `stats` reported `history_floor: 0`, which every reader
            // prints as "complete, nothing pruned", and a commit index a caller
            // already held resolved to an unrelated frame — a plausible graph,
            // not an error (ledger row 80).
            //
            // The floor is persisted FIRST, for the reason the retention sweep
            // gives: a crash after it leaves the old WAL and any archives on
            // disk under a floor that already disowns them. The next open
            // removes the archives as orphans and replays the WAL over the
            // snapshot idempotently. No number from before the snapshot is
            // answered in that state: all of them are below the floor.
            //
            // What that state does do is count the old WAL's frames again,
            // above the floor, until the next truncating snapshot — the same
            // history under new numbers, and a total inflated by its length.
            // Never a reused number, but not the original ones either. Closing
            // that needs the floor to land with the snapshot in one write,
            // which is a format change (ledger row 80, "what is left").
            let discarded_through = self.wal_total_commits()?;
            if discarded_through > self.wal_horizon_floor {
                self.fs.write_horizon_floor(discarded_through)?;
                self.wal_horizon_floor = discarded_through;
            }
            let mut baseline_wal: Vec<u8> = Vec::new();
            // The multiplicity opt-in is a declaration like the two below it,
            // and it is re-emitted for the same reason: truncation must not
            // silently opt the store back out and stop counting.
            if self.multiplicity {
                baseline_wal
                    .extend_from_slice(&encode_record(&core_storage::wal::MULTIPLICITY_ENABLED));
            }
            for (label, field) in self.fulltext.enabled_pairs() {
                let rec = WalRecord::EnableFulltext {
                    label: label.clone(),
                    field: field.clone(),
                };
                baseline_wal.extend_from_slice(&encode_record(&rec));
            }
            for (label, field) in self.prop_index.equality_pairs() {
                let rec = WalRecord::EnableIndex {
                    label: label.clone(),
                    field: field.clone(),
                };
                baseline_wal.extend_from_slice(&encode_record(&rec));
            }
            // Only a store that holds `EnableRangeIndex` records has any, so a
            // store without them gets no V11 record here.
            for (name, (label, field)) in self.prop_index.range_indexes() {
                let rec = WalRecord::EnableRangeIndex {
                    name: name.clone(),
                    label: label.clone(),
                    field: field.clone(),
                };
                baseline_wal.extend_from_slice(&encode_record(&rec));
            }
            // After the indexes, so each constraint finds its backing index
            // already declared. Only a store that holds `CreateConstraint`
            // records has any, so a store without them gets no V11 record here.
            for c in &self.constraints {
                let rec = WalRecord::CreateConstraint {
                    name: c.name.clone(),
                    label: c.label.clone(),
                    field: c.field.clone(),
                };
                baseline_wal.extend_from_slice(&encode_record(&rec));
            }
            stopped(SnapshotStop::BeforeWalReplacement)?;
            self.durable_wal.replaced();
            self.fs.write_atomic(FileId::Wal, &baseline_wal)?;
            // Every archive now ends at or below the floor. They are the
            // history this snapshot was asked to discard; removing them here is
            // what `cleanup_orphaned_archives` would do at the next open.
            for n in self.fs.list_archives()? {
                if n <= self.wal_horizon_floor {
                    self.fs.delete_archive(n)?;
                }
            }
            // This branch discards history rather than archiving it: the frames
            // the map describes are gone, and every entry it holds is below the
            // floor. A date before the floor has no commit to resolve to.
            //
            // A store that cannot answer a date says so by name
            // (`NoRecordedTime`). That is the honest state after discarding the
            // history the dates addressed.
            self.commit_times = core_storage::commit_times::CommitTimes::default();
            self.rewrite_commit_times();
        }
        // The WAL is the one that goes with the snapshot now. The intent has
        // done its work and must not outlive it.
        if replaces_wal {
            stopped(SnapshotStop::AfterWalReplacement)?;
            self.fs.write_atomic(FileId::SnapshotIntent, &[])?;
            self.intent_unsettled = false;
        }
        // After snapshot the overlay may have changed (V8 merge path clears
        // self.topo and self.props). Refresh the MVCC fold so future readers
        // see the post-snapshot state rather than stale overlay data.
        self.fold_after_snapshot();
        // We wrote the snapshot and (unless keep_wal) replaced the WAL, so both
        // markers this handle uses to detect other processes' work must be
        // re-taken from disk. Skipping this would make our own snapshot look
        // like a peer's on the next staleness check and force a needless
        // reload.
        self.wal_consumed = self.fs.wal_len().map_err(GraphError::Io)?;
        // A snapshot can replace the live WAL with a baseline, which has its
        // own frames. Re-derive the frame cursor from what the store now holds
        // (the floor, plus what is reachable above it) rather than carrying
        // the pre-snapshot count forward. `wal_total_commits` is the same
        // sequence the history surfaces index, and the snapshot has already
        // paid a far larger cost than one decode.
        self.wal_frames_written = self.wal_total_commits()?;
        self.snapshot_ident = self.fs.snapshot_ident().map_err(GraphError::Io)?;
        self.wal_ident = self.fs.wal_ident().map_err(GraphError::Io)?;
        self.durable_wal
            .snapshot_is(self.snapshot_ident, self.wal_ident);
        // A replacement WAL went in through `write_atomic`, which fsyncs it:
        // all of it is durable.
        if self.durable_wal.get().generation != generation_at_start {
            self.note_wal_synced();
        }
        Ok(())
    }
}

/// What a batch node insert does when its key is already taken.
///
/// A mirror rebuild writes a frame onto a store that already has content, so
/// "the key exists" is a routine answer rather than a failure. The decision is
/// made during the batch's existing validate pass, from one id-map lookup per
/// row, so the frame stays atomic and re-ingest stays O(n).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnConflict {
    /// Refuse the whole frame with [`GraphError::DuplicateKey`]. The default,
    /// and the only behaviour before v0.6.10.
    #[default]
    Error,
    /// Leave the stored node exactly as it is — properties, label and edges —
    /// and count it in [`BatchOutcome::skipped`].
    Skip,
    /// Keep the key and make the node's properties **exactly** the supplied
    /// props: supplied fields are set, fields absent from the supplied props
    /// are removed. A supplied label that differs from the stored one, and a
    /// supplied `ns` that would move the node, are row errors — relabelling is
    /// [`GraphDb::rename_node`], not a side effect of a rebuild.
    ///
    /// Two properties are outside "exactly", both because they are not the
    /// caller's to supply:
    ///
    /// - `ns` is immutable, so an omitted `ns` leaves the node where it is
    ///   rather than moving it to `default`;
    /// - a property a **view** owns is kept, not removed. Supplying one is a
    ///   row error, so omitting it cannot be a request to delete it, and
    ///   refusing the row instead would make `Replace` impossible for the whole
    ///   population a view has written to. Each such field kept is counted in
    ///   [`BatchOutcome::kept_view_owned`] — the row is still `replaced` and
    ///   still raises no row error, so that count is the only signal a caller
    ///   gets that the stored node carries a field its frame did not describe.
    Replace,
}

/// What one [`OnConflict::Replace`] row resolves to.
///
/// The property writes that make the node exactly the supplied props —
/// `Some(value)` is a set, `None` a removal — paired with how many view-owned
/// fields the row kept instead of removing, which is the one way the result is
/// not exactly the supplied props. See [`MutPreview::plan_replace`].
type ReplacePlan = (Vec<(String, Option<Value>)>, usize);

/// What one committed batch did.
///
/// [`BatchBuilder::commit`] returns the first two fields as a tuple; the rest
/// exist for [`OnConflict`] and are always zero / empty without it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchOutcome {
    /// Node records actually written.
    pub nodes_inserted: usize,
    /// Edge records actually written. A duplicate edge is a silent no-op under
    /// every policy — adjacency is a set — and is not counted.
    pub edges_inserted: usize,
    /// Rows whose key was taken and whose policy was [`OnConflict::Skip`].
    pub skipped: usize,
    /// Rows whose key was taken and whose policy was [`OnConflict::Replace`].
    pub replaced: usize,
    /// View-owned properties an [`OnConflict::Replace`] row **kept** although
    /// the caller did not supply them — counted per field, so one row that
    /// keeps two contributes two.
    ///
    /// This is the one respect in which `Replace` does not make a node's props
    /// exactly the supplied ones (see [`OnConflict::Replace`]). Those rows
    /// still count in `replaced` and still raise no `row_errors`, because
    /// nothing went wrong: a view's property is not the caller's to supply or
    /// to remove. A mirror rebuild that needs its copy to be byte-exact reads
    /// this to learn that the store kept fields its frame did not describe.
    pub kept_view_owned: usize,
    /// `(row, why)` for rows an [`OnConflict::Replace`] refused. `row` counts
    /// node-insert ops in this batch from zero, which for a caller that queues
    /// its nodes in order is the index of the offending node. The rest of the
    /// frame still commits; the refused row changes nothing.
    pub row_errors: Vec<(usize, String)>,
}

/// The `(nodes_inserted, edges_inserted)` pair every pre-0.6.10 commit entry
/// point returns. Keeps those signatures unchanged now that the validate pass
/// produces a [`BatchOutcome`].
fn inserted_pair(outcome: BatchOutcome) -> (usize, usize) {
    (outcome.nodes_inserted, outcome.edges_inserted)
}

/// The first format-V11 value type (`Date`, `LocalDateTime`, `DateTime`,
/// `Duration`, `Binary`) a WAL record would write, at any depth.
///
/// A 0.9.x reader cannot decode a frame carrying one and stops replaying
/// there, so such a value reaches the WAL only behind a V11 snapshot (see
/// [`rec_is_v11`]). The match names every record kind so a new kind that
/// carries a value cannot be added without deciding here.
fn v11_only_value(rec: &WalRecord) -> Option<&'static str> {
    match rec {
        WalRecord::InsertNode { props, .. } => props
            .iter()
            .find_map(|(_, v)| core_storage::v11_only_type(v)),
        WalRecord::InsertNodeId { props, .. } => props
            .iter()
            .find_map(|(_, v)| core_storage::v11_only_type(v)),
        WalRecord::SetProp { value, .. } | WalRecord::SetPropId { value, .. } => {
            core_storage::v11_only_type(value)
        }
        WalRecord::Batch(inner) => inner.iter().find_map(v11_only_value),
        WalRecord::InsertEdge { .. }
        | WalRecord::CreateRule { .. }
        | WalRecord::DeleteRule { .. }
        | WalRecord::RemoveProp { .. }
        | WalRecord::DeleteEdge { .. }
        | WalRecord::DeleteNode { .. }
        | WalRecord::RebuildRule { .. }
        | WalRecord::CreateView { .. }
        | WalRecord::DeleteView { .. }
        | WalRecord::EnableFulltext { .. }
        | WalRecord::DisableFulltext { .. }
        | WalRecord::InsertEdgeId { .. }
        | WalRecord::Intern { .. }
        | WalRecord::DerivedEdgeAdded { .. }
        | WalRecord::DerivedEdgeRetracted { .. }
        | WalRecord::RenameNode { .. }
        | WalRecord::EnableIndex { .. }
        | WalRecord::DisableIndex { .. }
        | WalRecord::SetEdgeCount { .. }
        | WalRecord::CreateConstraint { .. }
        | WalRecord::DropConstraint { .. }
        | WalRecord::EnableRangeIndex { .. }
        | WalRecord::DisableRangeIndex { .. } => None,
    }
}

/// One `Err` per submission of a group refused as a whole by `e`: the first
/// submission gets `e` itself, the rest its text.
pub(crate) fn group_refusal<T>(e: GraphError, n: usize) -> Vec<Result<T>> {
    let detail = format!("group refused: {e}");
    let mut out = Vec::with_capacity(n);
    out.push(Err(e));
    for _ in 1..n {
        out.push(Err(GraphError::Io(std::io::Error::other(detail.clone()))));
    }
    out.truncate(n);
    out
}

/// The declarations a snapshot's section 18 carries, as the records that make
/// them, in the order a baseline WAL writes them: equality indexes, range
/// indexes, then constraints, so each constraint finds its index declared.
fn declarations_as_records(d: &core_storage::v8::SnapshotDeclarations) -> Vec<WalRecord> {
    let mut recs = Vec::new();
    for (label, field) in &d.indexes {
        recs.push(WalRecord::EnableIndex {
            label: label.clone(),
            field: field.clone(),
        });
    }
    for (name, label, field) in &d.range_indexes {
        recs.push(WalRecord::EnableRangeIndex {
            name: name.clone(),
            label: label.clone(),
            field: field.clone(),
        });
    }
    for c in &d.constraints {
        recs.push(WalRecord::CreateConstraint {
            name: c.name.clone(),
            label: c.label.clone(),
            field: c.field.clone(),
        });
    }
    recs
}

/// Whether `rec` is a V11-only record: it carries a format-V11 value (see
/// [`v11_only_value`]), or it is a format-V11 record — a unique-constraint
/// declaration or withdrawal (discriminants 24 and 25) or a range-index one
/// (26 and 27). A 0.9.x reader can decode none of them, so the first one a
/// store writes moves it to the V11 snapshot format (contract C9).
fn rec_is_v11(rec: &WalRecord) -> bool {
    match rec {
        WalRecord::CreateConstraint { .. }
        | WalRecord::DropConstraint { .. }
        | WalRecord::EnableRangeIndex { .. }
        | WalRecord::DisableRangeIndex { .. } => true,
        WalRecord::Batch(inner) => recs_are_v11(inner),
        _ => v11_only_value(rec).is_some(),
    }
}

/// Whether any of `recs` is a V11-only record ([`rec_is_v11`]), recursing into
/// `Batch` inners.
fn recs_are_v11(recs: &[WalRecord]) -> bool {
    recs.iter().any(rec_is_v11)
}

/// The first `DateTime` offset in `rec`, at any depth, outside the ±18:00 a
/// `DateTime` holds. Every surface that parses one refuses it already; this
/// is the write-path check for a value built directly.
fn out_of_range_offset(rec: &WalRecord) -> Option<i32> {
    fn bad(v: &Value) -> Option<i32> {
        match v {
            Value::DateTime { offset_seconds, .. }
                if offset_seconds.unsigned_abs()
                    > core_storage::MAX_OFFSET_SECONDS.unsigned_abs() =>
            {
                Some(*offset_seconds)
            }
            Value::List(items) => items.iter().find_map(bad),
            Value::Map(m) => m.values().find_map(bad),
            _ => None,
        }
    }
    match rec {
        WalRecord::InsertNode { props, .. } => props.iter().find_map(|(_, v)| bad(v)),
        WalRecord::InsertNodeId { props, .. } => props.iter().find_map(|(_, v)| bad(v)),
        WalRecord::SetProp { value, .. } | WalRecord::SetPropId { value, .. } => bad(value),
        WalRecord::Batch(inner) => inner.iter().find_map(out_of_range_offset),
        _ => None,
    }
}

/// The first [`Value::Binary`] in `rec` longer than [`BINARY_MAX_BYTES`], at any
/// depth, as the property it is written to and its length.
fn oversized_binary(rec: &WalRecord, syms: &Interner) -> Option<(String, usize)> {
    fn too_big(v: &Value) -> Option<usize> {
        match v {
            Value::Binary(b) if b.len() > BINARY_MAX_BYTES => Some(b.len()),
            Value::List(items) => items.iter().find_map(too_big),
            Value::Map(m) => m.values().find_map(too_big),
            _ => None,
        }
    }
    match rec {
        WalRecord::InsertNode { props, .. } => props
            .iter()
            .find_map(|(f, v)| too_big(v).map(|n| (f.clone(), n))),
        WalRecord::InsertNodeId { props, .. } => props.iter().find_map(|(f, v)| {
            too_big(v).map(|n| (syms.resolve(*f).unwrap_or_default().to_string(), n))
        }),
        WalRecord::SetProp { field, value, .. } => too_big(value).map(|n| (field.clone(), n)),
        WalRecord::SetPropId { field, value, .. } => {
            too_big(value).map(|n| (syms.resolve(*field).unwrap_or_default().to_string(), n))
        }
        WalRecord::Batch(inner) => inner.iter().find_map(|r| oversized_binary(r, syms)),
        _ => None,
    }
}

/// One entry of a frame the validate pass has decided on, before
/// [`GraphDb::rewrite_wal_dense_planned`] turns it into dense-id records.
///
/// Almost every entry is already a finished [`WalRecord`]. The exception is a
/// duplicate edge insert: its count names a dense triple, and on the batch path
/// the endpoints and the edge type may all be created by earlier records in the
/// *same* frame, so no id for them exists until the dense rewrite allocates it.
/// Carrying the keys this far and resolving them there is what lets the count
/// survive the shape a mirror rebuild writes (defect #24).
enum PlannedRec {
    Rec(WalRecord),
    DuplicateCount {
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
}

/// Queued mutation for a [`BatchBuilder`] or [`GraphDb::commit_group`].
///
/// The `submit_batch` / `commit_group` APIs accept `Vec<BatchOp>` so that
/// callers can build a set of mutations without holding `&mut GraphDb` and
/// hand them off to the group-committing writer for durable, batched I/O.
pub enum BatchOp {
    InsertNode {
        label: String,
        key: String,
        props: Vec<(String, Value)>,
    },
    InsertEdge {
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
    SetProp {
        key: String,
        field: String,
        value: Value,
    },
    RemoveProp {
        key: String,
        field: String,
    },
    DeleteEdge {
        edge_type: String,
        src_key: String,
        dst_key: String,
    },
    DeleteNode {
        key: String,
    },
    CreateRule(RuleDef),
    DeleteRule {
        name: String,
    },
    /// Rename a node's key. Validated: old must exist, new must not.
    RenameNode {
        old_key: String,
        new_key: String,
    },
    /// Insert an edge, auto-creating any missing endpoint as a plain node with
    /// `placeholder_label` and no props. Rules fire and last-change is updated
    /// for each created endpoint (normal InsertNode semantics in the batch frame).
    InsertEdgeUpsert {
        edge_type: String,
        src_key: String,
        dst_key: String,
        placeholder_label: String,
    },
    /// Insert `key`, or — when the key is already taken — do what `on_conflict`
    /// says. Queued by [`BatchBuilder::insert_node_on_conflict`]; `Error`
    /// queues a plain [`BatchOp::InsertNode`] instead, so this variant only
    /// ever carries `Skip` or `Replace`.
    InsertNodeOnConflict {
        label: String,
        key: String,
        props: Vec<(String, Value)>,
        on_conflict: OnConflict,
    },
}

/// Three-way node visibility status used by `check_single_op_authz`.
enum NodeAuthzStatus {
    /// Node exists in the store and is in the role's read mask.
    Visible(String), // carries the node's label
    /// Node exists in the store but is NOT in the role's read mask.
    Hidden,
    /// Node does not exist in the store.
    Absent,
}

/// An edge as a batch names it: `(edge type, source key, destination key)`.
type EdgeKey = (String, String, String);

/// A set of batch edges that can also be found by either endpoint.
///
/// A node delete or rename in a batch has to find the edges the batch
/// already added or deleted at that node. Filtering the whole set for them
/// made a batch of N edge inserts and M node deletes cost O(N × M) (ledger
/// row 130); the endpoint index finds them in O(log N + edges at the node).
#[derive(Default)]
struct EdgeKeys {
    edges: BTreeSet<EdgeKey>,
    /// `(endpoint key, edge)` for each endpoint of each edge in `edges`: once
    /// for a self-loop.
    by_endpoint: BTreeSet<(String, EdgeKey)>,
}

#[cfg(test)]
thread_local! {
    /// Entries [`EdgeKeys::take_touching`] visited, for the test that pins it
    /// to the edges at the node.
    static EDGE_KEYS_VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl EdgeKeys {
    fn contains(&self, k: &EdgeKey) -> bool {
        self.edges.contains(k)
    }

    fn insert(&mut self, k: EdgeKey) {
        if self.edges.contains(&k) {
            return;
        }
        self.by_endpoint.insert((k.1.clone(), k.clone()));
        if k.2 != k.1 {
            self.by_endpoint.insert((k.2.clone(), k.clone()));
        }
        self.edges.insert(k);
    }

    fn remove(&mut self, k: &EdgeKey) {
        if !self.edges.remove(k) {
            return;
        }
        self.by_endpoint.remove(&(k.1.clone(), k.clone()));
        self.by_endpoint.remove(&(k.2.clone(), k.clone()));
    }

    /// Remove and return every edge with `key` at either end.
    fn take_touching(&mut self, key: &str) -> Vec<EdgeKey> {
        let start = (
            key.to_string(),
            (String::new(), String::new(), String::new()),
        );
        let touching: Vec<EdgeKey> = self
            .by_endpoint
            .range(start..)
            // Counts every entry the walk reads, the one that ends it too.
            .inspect(|_| {
                #[cfg(test)]
                EDGE_KEYS_VISITED.with(|v| v.set(v.get() + 1));
            })
            .take_while(|(endpoint, _)| endpoint == key)
            .map(|(_, edge)| edge.clone())
            .collect();
        for edge in &touching {
            self.remove(edge);
        }
        touching
    }
}

/// Overlay of ops already accepted earlier in the same batch. Never written
/// back to the database — validation only.
#[derive(Default)]
struct Overlay {
    extra_keys: BTreeSet<String>,
    /// Label of each node inserted earlier in this batch. The store does not
    /// have these keys yet, so `label_of` cannot answer for them, and
    /// `OnConflict::Replace` has to compare labels.
    extra_labels: BTreeMap<String, String>,
    deleted_keys: BTreeSet<String>,
    /// `new key → store key` for every node an earlier op in this batch
    /// renamed. Such a key is in `extra_keys`, but unlike one a batch inserts
    /// it has a stored node behind it — under the old key until the batch
    /// commits — and its properties, label and edges are read through this.
    renamed_from: BTreeMap<String, String>,
    extra_props: BTreeMap<(String, String), Value>,
    removed_props: BTreeSet<(String, String)>,
    extra_edges: EdgeKeys,
    deleted_edges: EdgeKeys,
    extra_rules: BTreeSet<String>,
    deleted_rules: BTreeSet<String>,
    /// `rule name → (via_edge, edge_type)` for every via-hop rule accepted
    /// earlier in this batch. Feeds the rule-chain cycle check, which otherwise
    /// sees only the rules already committed to the engine. Keyed by name so a
    /// later `DeleteRule` in the same batch drops the arc with the rule.
    extra_rule_arcs: BTreeMap<String, (String, String)>,
}

impl Overlay {
    /// Remove and return every overlay prop of `key`.
    ///
    /// A range lookup on the ordered `(key, field)` pairs, not a `retain`:
    /// `retain` walks every overlay entry, so a batch of N ops cost O(N²)
    /// (ledger row 118). Same entries, O(log N + entries of this key).
    fn take_props_of(&mut self, key: &str) -> Vec<(String, Value)> {
        let pairs: Vec<(String, String)> = self
            .extra_props
            .range((key.to_string(), String::new())..)
            .take_while(|((k, _), _)| k == key)
            .map(|(pair, _)| pair.clone())
            .collect();
        pairs
            .into_iter()
            .filter_map(|pair| {
                let value = self.extra_props.remove(&pair)?;
                Some((pair.1, value))
            })
            .collect()
    }

    /// Remove and return every removed-prop tombstone of `key` (range lookup;
    /// see [`Overlay::take_props_of`]).
    fn take_removed_props_of(&mut self, key: &str) -> Vec<String> {
        let pairs: Vec<(String, String)> = self
            .removed_props
            .range((key.to_string(), String::new())..)
            .take_while(|(k, _)| k == key)
            .cloned()
            .collect();
        for pair in &pairs {
            self.removed_props.remove(pair);
        }
        pairs.into_iter().map(|(_, field)| field).collect()
    }
}

/// Read-only view of live db state plus a batch overlay. Shared by single-op
/// public methods (empty overlay) and `commit_batch`.
struct MutPreview<'a, F: Fs> {
    db: &'a GraphDb<F>,
    overlay: Overlay,
}

/// Shortest path from `start` to `target` following `arcs` (`from → to`), or
/// `None` if `target` is unreachable.
///
/// Used for rule-chain cycle detection, where an arc is "a rule hops over
/// `from` and writes `to`". Breadth-first over BTree-ordered adjacency, so the
/// reported path is stable for a given rule set, and iterative so a pathological
/// rule graph cannot overflow the stack.
fn find_cycle_through(arcs: &[(String, String)], start: &str, target: &str) -> Option<Vec<String>> {
    let mut adj: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (from, to) in arcs {
        adj.entry(from.as_str()).or_default().insert(to.as_str());
    }
    let mut parent: BTreeMap<&str, &str> = BTreeMap::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    visited.insert(start);
    queue.push_back(start);
    while let Some(node) = queue.pop_front() {
        if node == target {
            let mut path = vec![node.to_string()];
            let mut cur = node;
            while let Some(&p) = parent.get(cur) {
                path.push(p.to_string());
                cur = p;
            }
            path.reverse();
            return Some(path);
        }
        for &next in adj.get(node).into_iter().flatten() {
            if visited.insert(next) {
                parent.insert(next, node);
                queue.push_back(next);
            }
        }
    }
    None
}

impl<'a, F: Fs> MutPreview<'a, F> {
    fn new(db: &'a GraphDb<F>) -> Self {
        Self {
            db,
            overlay: Overlay::default(),
        }
    }

    fn has_key(&self, key: &str) -> bool {
        if self.overlay.extra_keys.contains(key) {
            return true;
        }
        if self.overlay.deleted_keys.contains(key) {
            return false;
        }
        self.db.ids.get(key).is_some()
    }

    /// The key the store holds `key`'s node under, or `None` when the batch
    /// sees no stored node there: `key` is deleted, or was first inserted in
    /// this batch. A key an earlier op renamed stands for the node still
    /// stored under its old key.
    fn store_key<'k>(&'k self, key: &'k str) -> Option<&'k str> {
        if self.overlay.deleted_keys.contains(key) {
            return None;
        }
        if let Some(origin) = self.overlay.renamed_from.get(key) {
            return Some(origin.as_str());
        }
        if self.overlay.extra_keys.contains(key) {
            return None;
        }
        Some(key)
    }

    fn has_prop(&self, key: &str, field: &str) -> bool {
        if !self.has_key(key) {
            return false;
        }
        let k = (key.to_string(), field.to_string());
        if self.overlay.removed_props.contains(&k) {
            return false;
        }
        if self.overlay.extra_props.contains_key(&k) {
            return true;
        }
        // Fresh identity (first insert in this batch, or delete+reinsert):
        // ignore props still sitting on the soon-to-be-tombstoned slot.
        let Some(stored) = self.store_key(key) else {
            return false;
        };
        self.db.get_prop(stored, field).is_some()
    }

    fn has_edge(&self, edge_type: &str, src_key: &str, dst_key: &str) -> bool {
        let k = (
            edge_type.to_string(),
            src_key.to_string(),
            dst_key.to_string(),
        );
        if self.overlay.deleted_edges.contains(&k) {
            return false;
        }
        if self.overlay.extra_edges.contains(&k) {
            return true;
        }
        // A key created in this batch (including reinsert) has no db edges.
        let (Some(src_key), Some(dst_key)) = (self.store_key(src_key), self.store_key(dst_key))
        else {
            return false;
        };
        let Some(src) = self.db.ids.get(src_key) else {
            return false;
        };
        let Some(dst) = self.db.ids.get(dst_key) else {
            return false;
        };
        let Some(sym) = self.db.syms.get(edge_type) else {
            return false;
        };
        self.db
            .topo_view()
            .neighbors(sym, Direction::Out, src)
            .binary_search(&dst)
            .is_ok()
    }

    fn has_rule(&self, name: &str) -> bool {
        if self.overlay.extra_rules.contains(name) {
            return true;
        }
        if self.overlay.deleted_rules.contains(name) {
            return false;
        }
        self.db.engine.rules().any(|r| r.name == name)
    }

    fn is_rule_owned(&self, edge_type: &str, src_key: &str, dst_key: &str) -> bool {
        let (Some(src_key), Some(dst_key)) = (self.store_key(src_key), self.store_key(dst_key))
        else {
            return false;
        };
        let Some(src) = self.db.ids.get(src_key) else {
            return false;
        };
        let Some(dst) = self.db.ids.get(dst_key) else {
            return false;
        };
        let Some(et) = self.db.syms.get(edge_type) else {
            return false;
        };
        // extra_rules is deliberately not consulted: a CreateRule earlier in
        // this batch has not fired, so it contributes no provenance. That is
        // the documented rule-window gap (see GraphDb::batch).
        if self.overlay.deleted_rules.is_empty() {
            return self.db.engine.is_owned(et, src, dst);
        }
        for (rule, triples) in self.db.engine.provenance() {
            if self.overlay.deleted_rules.contains(rule) {
                continue;
            }
            if triples.contains(&(et, src, dst)) {
                return true;
            }
        }
        false
    }

    /// The refusals a node creation makes, in the order it makes them.
    ///
    /// A view owns its property, and creating a node that carries one is a
    /// write to it exactly as `set_prop` is — so it is refused here, at the one
    /// choke-point `GraphDb::insert_node`, `BatchOp::InsertNode` and the
    /// no-conflict arm of `BatchOp::InsertNodeOnConflict` all pass through.
    ///
    /// Leaving creation exempt was not harmless. The value was stored and
    /// served: a created node the view has no reason to revisit keeps the
    /// caller's number for the life of the handle, and the backfill at the next
    /// open overwrites it — so the store answered `deg = 777` before a restart
    /// and `deg = 0` after, for a property every other surface calls read-only.
    /// It also split one op two ways: supplying a view-owned field under
    /// `OnConflict::Replace` was already a row error on a taken key while the
    /// same field on a fresh key was accepted.
    ///
    /// Checked before the key, like [`MutPreview::prepare_remove_prop`], so the
    /// answer does not depend on whether the key exists.
    fn check_insert_node(&self, key: &str, props: &[(String, Value)]) -> Result<()> {
        for (field, _) in props {
            if let Some(view_name) = self.db.view_store.view_for_prop(field) {
                return Err(GraphError::ViewPropReadOnly {
                    view_name: view_name.to_string(),
                });
            }
        }
        if self.has_key(key) {
            Err(GraphError::DuplicateKey { key: key.into() })
        } else {
            Ok(())
        }
    }

    fn check_live_key(&self, key: &str) -> Result<()> {
        if self.has_key(key) {
            Ok(())
        } else {
            Err(GraphError::KeyNotFound { key: key.into() })
        }
    }

    fn prepare_insert_edge(&self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        for k in [src_key, dst_key] {
            if !self.has_key(k) {
                return Err(GraphError::KeyNotFound { key: k.into() });
            }
        }
        if self.is_rule_owned(edge_type, src_key, dst_key) {
            return Err(GraphError::RuleOwned {
                detail: format!("edge {edge_type} {src_key}→{dst_key} is rule-owned"),
            });
        }
        // A user-written edge stays inside one namespace. Derived edges do not
        // come through here — the engine adds them directly — and the rule
        // scoping check is what keeps those pure.
        let src_ns = self.namespace_in_batch(src_key);
        let dst_ns = self.namespace_in_batch(dst_key);
        if src_ns != dst_ns {
            return Err(GraphError::CrossNamespace {
                src: src_key.to_string(),
                src_ns,
                dst: dst_key.to_string(),
                dst_ns,
            });
        }
        Ok(!self.has_edge(edge_type, src_key, dst_key))
    }

    fn prepare_remove_prop(&self, key: &str, field: &str) -> Result<bool> {
        // `id` is not removable from a node whose `id` is its key (ledger row 128).
        // Stated first so the answer does not depend on any view.
        if field == "id" {
            check_key_property_removal(key, self.prop_value(key, "id").as_ref())?;
        }
        // A view owns its property, and the refusal has to live here rather
        // than on `GraphDb::remove_prop`: `BatchOp::RemoveProp` never meets
        // that one, and it is what the HTTP `DELETE /node/{key}/prop/{field}`
        // route, `Batch::remove_prop` and the CLI all submit. This is the one
        // choke-point every removal passes, exactly as it is for `ns` below.
        // Checked before the key, so the answer does not depend on whether the
        // key exists — which is also what `GraphDb::remove_prop` answered when
        // it carried the only copy of this guard.
        if let Some(view_name) = self.db.view_store.view_for_prop(field) {
            return Err(GraphError::ViewPropReadOnly {
                view_name: view_name.to_string(),
            });
        }
        self.check_live_key(key)?;
        // Removing `ns` is changing the namespace — to `default`, the namespace
        // an absent property names. It goes through this one choke-point and NOT
        // through `rewrite_wal_dense` (a `RemoveProp` needs no dense rewrite), so
        // the immutability rule has to be stated here as well. Without it the
        // node silently lands in `default` on the next open: the cross-namespace
        // edge guard is defeated and a default-bound role reads a tenant's node.
        if field == NS_PROP {
            let from = self.namespace_in_batch(key);
            if from != NS_DEFAULT {
                return Err(GraphError::NamespaceImmutable {
                    key: key.to_string(),
                    from,
                    to: NS_DEFAULT.to_string(),
                });
            }
            // Already in `default`: the removal changes no namespace. It is the
            // no-op `set_prop` to the current namespace is, not an error.
            return Ok(false);
        }
        Ok(self.has_prop(key, field))
    }

    fn prepare_delete_edge(&self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        for k in [src_key, dst_key] {
            if !self.has_key(k) {
                return Err(GraphError::KeyNotFound { key: k.into() });
            }
        }
        // Provenance-owned OR a live rule would derive this pair. User-first
        // edges that a later rule matches are not in `owned`, but deleting
        // them would leave a hole `rebuild_rule` immediately fills.
        if self.is_rule_owned(edge_type, src_key, dst_key) {
            return Err(GraphError::RuleOwned {
                detail: format!(
                    "edge {edge_type} {src_key}→{dst_key} is rule-owned; \
                     delete or change the owning rule"
                ),
            });
        }
        if self.would_derive(edge_type, src_key, dst_key) {
            return Err(GraphError::RuleOwned {
                detail: format!(
                    "edge {edge_type} {src_key}→{dst_key} is rule-owned; \
                     delete or change the owning rule, or a live rule would re-derive it"
                ),
            });
        }
        Ok(self.has_edge(edge_type, src_key, dst_key))
    }

    /// True if any live rule (minus overlay-deleted names) would derive
    /// `(edge_type, src, dst)` from current overlay-visible props/labels.
    /// CreateRule names in `extra_rules` are ignored — same documented
    /// same-batch rule-window as [`Self::is_rule_owned`].
    ///
    /// Mirrored by `guard_matches` in `memory/forget.rs`, which names the
    /// rules this refused for: change the two together (ledger row 67).
    fn would_derive(&self, edge_type: &str, src_key: &str, dst_key: &str) -> bool {
        if src_key == dst_key {
            return false;
        }
        let Some(src_label) = self.label_of(src_key) else {
            return false;
        };
        let Some(dst_label) = self.label_of(dst_key) else {
            return false;
        };
        for rule in self.db.engine.rules() {
            if self.overlay.deleted_rules.contains(&rule.name) {
                continue;
            }
            if rule.edge_type != edge_type {
                continue;
            }
            if rule.src_label != src_label || rule.dst_label != dst_label {
                continue;
            }
            let src_props = |f: &str| self.prop_value(src_key, f);
            let dst_props = |f: &str| self.prop_value(dst_key, f);
            let src_view = NodeView {
                key: src_key,
                props: &src_props,
            };
            let dst_view = NodeView {
                key: dst_key,
                props: &dst_props,
            };
            if evaluate(&rule.predicate, &src_view, &dst_view).is_some() {
                return true;
            }
        }
        false
    }

    fn label_of(&self, key: &str) -> Option<String> {
        if self.overlay.deleted_keys.contains(key) {
            return None;
        }
        // Fresh identities created in this batch have no stored label in the
        // overlay; they cannot be provenance-owned yet either. A key this
        // batch renamed has its label on the node stored under the old key.
        let stored = self
            .overlay
            .renamed_from
            .get(key)
            .map_or(key, String::as_str);
        let id = self.db.ids.get(stored)?;
        let sym = self.db.labels.get(id as usize).copied()?;
        if sym == u32::MAX {
            return None;
        }
        self.db.syms.resolve(sym).map(str::to_string)
    }

    /// The label `key` carries as this batch sees it — including a node
    /// inserted earlier in the same batch, which the store does not have yet.
    fn label_in_batch(&self, key: &str) -> Option<String> {
        if self.overlay.deleted_keys.contains(key) {
            return None;
        }
        if let Some(label) = self.overlay.extra_labels.get(key) {
            return Some(label.clone());
        }
        self.label_of(key)
    }

    /// The property writes that make `key`'s props exactly `props`, or why the
    /// row is refused.
    ///
    /// `Some(value)` is a set and `None` is a removal. `store_fields` is every
    /// field name the store knows, hoisted by the caller so a frame of N
    /// replaces reads the field list once rather than N times.
    ///
    /// The second half of the pair is how many view-owned fields this row kept
    /// rather than removed — the one part of "exactly the supplied props" that
    /// does not hold, and the caller's only signal that it did not.
    ///
    /// The refusals are row errors, not frame errors: a mirror rebuild should
    /// learn which of its rows disagree with the store without losing the rows
    /// that agree.
    fn plan_replace(
        &self,
        label: &str,
        key: &str,
        props: &[(String, Value)],
        store_fields: &[String],
    ) -> std::result::Result<ReplacePlan, String> {
        // A different label is a relabel, and a rebuild does not relabel: that
        // is `rename_node` or an explicit write, never a side effect here.
        let stored = self.label_in_batch(key).unwrap_or_default();
        if stored != label {
            return Err(format!(
                "node {key}: on_conflict=\"replace\" will not relabel {stored:?} to {label:?}; \
                 relabelling is rename_node or an explicit write"
            ));
        }
        // `ns` is immutable. Replace removes what the supplied props omit, so
        // an omitted `ns` is a move to `default` exactly as a different `ns` is
        // a move to that one; both are the same refusal.
        let from = self.namespace_in_batch(key);
        let to = match props.iter().find(|(field, _)| field == NS_PROP) {
            Some((_, Value::Str(ns))) => ns.clone(),
            Some((_, value)) => {
                return Err(format!(
                    "node {key}: {NS_PROP} must be a string naming a namespace, got {value:?}"
                ));
            }
            None => NS_DEFAULT.to_string(),
        };
        if to != from {
            return Err(format!(
                "node {key}: {NS_PROP} is immutable; on_conflict=\"replace\" cannot move it \
                 from {from:?} to {to:?}"
            ));
        }

        if let Some(why) = self.supplied_view_owned_prop(key, props) {
            return Err(why);
        }

        let supplied: BTreeSet<&str> = props.iter().map(|(field, _)| field.as_str()).collect();
        let mut writes = Vec::new();
        for (field, value) in props {
            // `ns` names the namespace the node is already in, so the write is
            // the no-op the dense-rewrite seam would drop anyway.
            if field == NS_PROP {
                continue;
            }
            // Already exactly this value: a rebuild of an unchanged row should
            // cost no WAL record.
            if self.prop_value(key, field).as_ref() == Some(value) {
                continue;
            }
            writes.push((field.clone(), Some(value.clone())));
        }
        // Everything the node still carries that the supplied props do not.
        // `ns` is never removed: it is immutable, and the check above has
        // already established the node stays where it is.
        let overlay_fields = self
            .overlay
            .extra_props
            .keys()
            .filter(|(k, _)| k == key)
            .map(|(_, field)| field.as_str());
        //
        // A view-owned field is filtered out rather than refused. It is not the
        // caller's to supply (supplying one is still the row error above) and
        // so it is not part of what "exactly the supplied ones" ranges over:
        // omitting it is not a request to delete it. Refusing here instead
        // would make `replace` impossible for every node a view has written to
        // — which on a store carrying a view is the whole population a mirror
        // rebuild has to cover.
        let omitted: BTreeSet<&str> = store_fields
            .iter()
            .map(String::as_str)
            .chain(overlay_fields)
            .filter(|field| {
                *field != NS_PROP
                    && !(*field == "id" && id_mirrors_key(key, self.prop_value(key, "id").as_ref()))
                    && !supplied.contains(field)
                    && self.has_prop(key, field)
            })
            .collect();
        // The view-owned half is kept, and counted: the row still commits and
        // still reports no error, so without this number a mirror rebuild is
        // told it got exactly what it asked for when it did not (defect #18).
        let (stale, kept): (Vec<&str>, Vec<&str>) = omitted
            .into_iter()
            .partition(|field| self.db.view_store.view_for_prop(field).is_none());
        writes.extend(stale.into_iter().map(|field| (field.to_string(), None)));
        Ok((writes, kept.len()))
    }

    /// The row error a supplied view-owned field earns, or `None`.
    ///
    /// Shared by [`MutPreview::plan_replace`] and the no-conflict arm of
    /// `BatchOp::InsertNodeOnConflict` so that one op answers a supplied
    /// view-owned field the same way whether or not the key was already taken.
    fn supplied_view_owned_prop(&self, key: &str, props: &[(String, Value)]) -> Option<String> {
        props.iter().find_map(|(field, _)| {
            self.db.view_store.view_for_prop(field).map(|view_name| {
                format!(
                    "node {key}: property {field:?} is owned by view {view_name:?} and is \
                     read-only"
                )
            })
        })
    }

    /// The namespace `key` is in as this batch sees it — including a node
    /// inserted earlier in the same batch, which the store does not have yet.
    fn namespace_in_batch(&self, key: &str) -> String {
        namespace_of_value(self.prop_value(key, NS_PROP).as_ref()).to_string()
    }

    fn prop_value(&self, key: &str, field: &str) -> Option<Value> {
        if !self.has_key(key) {
            return None;
        }
        let k = (key.to_string(), field.to_string());
        if self.overlay.removed_props.contains(&k) {
            return None;
        }
        if let Some(v) = self.overlay.extra_props.get(&k) {
            return Some(v.clone());
        }
        self.db.get_prop(self.store_key(key)?, field)
    }

    fn check_create_rule(&self, def: &RuleDef) -> Result<()> {
        def.validate()
            .map_err(|e| GraphError::RuleInvalid { detail: e })?;
        if self.has_rule(&def.name) {
            return Err(GraphError::RuleInvalid {
                detail: format!("rule {:?} already exists", def.name),
            });
        }
        // Rule-chain cycle rejection. Derived edges feed via-hop rules, so a
        // rule set forms a graph whose arcs are "hops over `via_edge`, writes
        // `edge_type`". A cycle in that graph is a rule set that would re-fire
        // itself forever; the engine's depth cap would silently truncate it
        // instead, leaving an arbitrary partial result. Reject it here, the one
        // place that sees the whole rule set.
        //
        // Rules accepted earlier in the same batch count too: the overlay
        // carries their arcs, so a cycle cannot be assembled one op at a time.
        if let Some(via) = def.via_edge.as_deref() {
            if via == def.edge_type {
                return Err(GraphError::RuleInvalid {
                    detail: format!("rule chain cycle: {} -> {}", via, def.edge_type),
                });
            }
            let mut arcs: Vec<(String, String)> = self
                .db
                .engine
                .rules()
                .filter(|r| !self.overlay.deleted_rules.contains(&r.name))
                .filter_map(|r| r.via_edge.clone().map(|v| (v, r.edge_type.clone())))
                .collect();
            arcs.extend(self.overlay.extra_rule_arcs.values().cloned());
            arcs.push((via.to_string(), def.edge_type.clone()));
            if let Some(path) = find_cycle_through(&arcs, &def.edge_type, via) {
                return Err(GraphError::RuleInvalid {
                    detail: format!("rule chain cycle: {} -> {}", via, path.join(" -> ")),
                });
            }
        }
        Ok(())
    }

    fn check_delete_rule(&self, name: &str) -> Result<()> {
        if self.has_rule(name) {
            Ok(())
        } else {
            Err(GraphError::RuleNotFound { name: name.into() })
        }
    }

    fn note_insert_node(&mut self, label: &str, key: &str, props: &[(String, Value)]) {
        self.overlay.deleted_keys.remove(key);
        self.overlay.renamed_from.remove(key);
        self.overlay.extra_keys.insert(key.to_string());
        self.overlay
            .extra_labels
            .insert(key.to_string(), label.to_string());
        self.overlay.take_props_of(key);
        self.overlay.take_removed_props_of(key);
        for (field, value) in props {
            self.overlay
                .extra_props
                .insert((key.to_string(), field.clone()), value.clone());
        }
    }

    fn note_insert_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) {
        let k = (
            edge_type.to_string(),
            src_key.to_string(),
            dst_key.to_string(),
        );
        self.overlay.deleted_edges.remove(&k);
        self.overlay.extra_edges.insert(k);
    }

    fn note_set_prop(&mut self, key: &str, field: &str, value: &Value) {
        let k = (key.to_string(), field.to_string());
        self.overlay.removed_props.remove(&k);
        self.overlay.extra_props.insert(k, value.clone());
    }

    fn note_remove_prop(&mut self, key: &str, field: &str) {
        let k = (key.to_string(), field.to_string());
        self.overlay.extra_props.remove(&k);
        self.overlay.removed_props.insert(k);
    }

    fn note_delete_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) {
        let k = (
            edge_type.to_string(),
            src_key.to_string(),
            dst_key.to_string(),
        );
        self.overlay.extra_edges.remove(&k);
        self.overlay.deleted_edges.insert(k);
    }

    fn note_delete_node(&mut self, key: &str) {
        self.overlay.extra_keys.remove(key);
        self.overlay.renamed_from.remove(key);
        self.overlay.deleted_keys.insert(key.to_string());
        self.overlay.take_props_of(key);
        self.overlay.take_removed_props_of(key);
        self.overlay.extra_edges.take_touching(key);
        self.overlay.deleted_edges.take_touching(key);
    }

    fn note_create_rule(&mut self, def: &RuleDef) {
        self.overlay.deleted_rules.remove(&def.name);
        self.overlay.extra_rules.insert(def.name.clone());
        // Rules accepted earlier in this batch are not in the engine yet, so
        // the cycle check would not see their arcs. Keep the arc, not just the
        // name, so a batch cannot smuggle in a cycle one op at a time.
        if let Some(via) = def.via_edge.clone() {
            self.overlay
                .extra_rule_arcs
                .insert(def.name.clone(), (via, def.edge_type.clone()));
        }
    }

    fn check_rename_node(&self, old: &str, new: &str) -> Result<()> {
        if !self.has_key(old) {
            return Err(GraphError::KeyNotFound { key: old.into() });
        }
        if self.has_key(new) {
            return Err(GraphError::DuplicateKey { key: new.into() });
        }
        Ok(())
    }

    fn note_rename_node(&mut self, old: &str, new: &str) {
        // The stored node `new` will stand for: the one `old` stood for if an
        // earlier op renamed it here, `old` itself if the store has it, and
        // none if `old` was first inserted in this batch.
        let origin = match self.overlay.renamed_from.remove(old) {
            Some(origin) => Some(origin),
            None if self.overlay.extra_keys.contains(old) => None,
            None => Some(old.to_string()),
        };
        // Mark old as deleted so subsequent batch ops cannot reference it.
        self.overlay.extra_keys.remove(old);
        self.overlay.deleted_keys.insert(old.to_string());
        // Mark new as extra so subsequent batch ops can reference it.
        self.overlay.deleted_keys.remove(new);
        self.overlay.extra_keys.insert(new.to_string());
        let new_str = new.to_string();
        // Migrate any overlay props from old key to new key.
        for (field, value) in self.overlay.take_props_of(old) {
            self.overlay
                .extra_props
                .insert((new.to_string(), field), value);
        }
        // Migrate removed_props.
        for field in self.overlay.take_removed_props_of(old) {
            self.overlay.removed_props.insert((new.to_string(), field));
        }
        if let Some(label) = self.overlay.extra_labels.remove(old) {
            self.overlay.extra_labels.insert(new_str.clone(), label);
        }
        // Edges the batch added or deleted follow the node to its new key.
        let follow = |k: String| if k == old { new_str.clone() } else { k };
        for set in [
            &mut self.overlay.extra_edges,
            &mut self.overlay.deleted_edges,
        ] {
            for (t, s, d) in set.take_touching(old) {
                set.insert((t, follow(s), follow(d)));
            }
        }
        if let Some(origin) = origin {
            self.overlay.renamed_from.insert(new_str, origin);
        }
    }

    fn note_delete_rule(&mut self, name: &str) {
        self.overlay.extra_rules.remove(name);
        // Drop its chain arc too: a rule created and then deleted in the same
        // batch must not make a later, legal rule look like a cycle.
        self.overlay.extra_rule_arcs.remove(name);
        self.overlay.deleted_rules.insert(name.to_string());
        // Treat the deleted rule's current provenance as gone so a later
        // delete_edge of those triples is a no-op (matches sequential).
        //
        // Provenance names nodes by the key the store holds them under; the
        // overlay names them as the batch does. A node an earlier op renamed
        // is recorded under its new key, or a later read of that key would go
        // through to the store and find the rule's edge still there.
        let now: BTreeMap<&str, &str> = self
            .overlay
            .renamed_from
            .iter()
            .map(|(new, stored)| (stored.as_str(), new.as_str()))
            .collect();
        if let Some(triples) = self.db.engine.provenance().get(name) {
            for &(et, s, d) in triples {
                let Some(etype) = self.db.syms.resolve(et) else {
                    continue;
                };
                let Some(src) = self.db.ids.key_of(s) else {
                    continue;
                };
                let Some(dst) = self.db.ids.key_of(d) else {
                    continue;
                };
                let src = now.get(src).copied().unwrap_or(src);
                let dst = now.get(dst).copied().unwrap_or(dst);
                let k = (etype.to_string(), src.to_string(), dst.to_string());
                self.overlay.extra_edges.remove(&k);
                self.overlay.deleted_edges.insert(k);
            }
        }
    }
}

/// Collects mutations and commits them as one WAL `Batch` frame.
///
/// Holds `&mut GraphDb` for its lifetime. Queue with the same method names
/// as [`GraphDb`]; call [`commit`](Self::commit) to validate, log, and apply.
/// See [`GraphDb::batch`] for validation and atomicity rules.
pub struct BatchBuilder<'a, F: Fs> {
    db: &'a mut GraphDb<F>,
    ops: Vec<BatchOp>,
}

impl<'a, F: Fs> BatchBuilder<'a, F> {
    pub fn insert_node(
        &mut self,
        label: &str,
        key: &str,
        props: Vec<(String, Value)>,
    ) -> &mut Self {
        self.ops.push(BatchOp::InsertNode {
            label: label.into(),
            key: key.into(),
            props,
        });
        self
    }

    /// Queue a node insert whose answer to a taken key is `on_conflict`.
    ///
    /// [`OnConflict::Error`] queues exactly the op [`insert_node`](Self::insert_node)
    /// does, so the default path is unchanged.
    pub fn insert_node_on_conflict(
        &mut self,
        label: &str,
        key: &str,
        props: Vec<(String, Value)>,
        on_conflict: OnConflict,
    ) -> &mut Self {
        self.ops.push(match on_conflict {
            OnConflict::Error => BatchOp::InsertNode {
                label: label.into(),
                key: key.into(),
                props,
            },
            on_conflict => BatchOp::InsertNodeOnConflict {
                label: label.into(),
                key: key.into(),
                props,
                on_conflict,
            },
        });
        self
    }

    pub fn insert_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> &mut Self {
        self.ops.push(BatchOp::InsertEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        });
        self
    }

    pub fn set_prop(&mut self, key: &str, field: &str, value: Value) -> &mut Self {
        self.ops.push(BatchOp::SetProp {
            key: key.into(),
            field: field.into(),
            value,
        });
        self
    }

    pub fn remove_prop(&mut self, key: &str, field: &str) -> &mut Self {
        self.ops.push(BatchOp::RemoveProp {
            key: key.into(),
            field: field.into(),
        });
        self
    }

    pub fn delete_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> &mut Self {
        self.ops.push(BatchOp::DeleteEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        });
        self
    }

    pub fn delete_node(&mut self, key: &str) -> &mut Self {
        self.ops.push(BatchOp::DeleteNode { key: key.into() });
        self
    }

    pub fn create_rule(&mut self, def: RuleDef) -> &mut Self {
        self.ops.push(BatchOp::CreateRule(def));
        self
    }

    pub fn delete_rule(&mut self, name: &str) -> &mut Self {
        self.ops.push(BatchOp::DeleteRule { name: name.into() });
        self
    }

    /// Queue a node-rename in this batch.
    ///
    /// Validation (old exists, new not taken) runs at commit time.
    pub fn rename_node(&mut self, old_key: &str, new_key: &str) -> &mut Self {
        self.ops.push(BatchOp::RenameNode {
            old_key: old_key.into(),
            new_key: new_key.into(),
        });
        self
    }

    /// Queue an edge insert with endpoint auto-creation.
    ///
    /// Any missing endpoint is created as a plain node `{key, label:
    /// placeholder_label, no props}` inside this batch frame. Rules fire and
    /// last-change is updated for each auto-created node.
    pub fn insert_edge_upsert(
        &mut self,
        edge_type: &str,
        src_key: &str,
        dst_key: &str,
        placeholder_label: &str,
    ) -> &mut Self {
        self.ops.push(BatchOp::InsertEdgeUpsert {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
            placeholder_label: placeholder_label.into(),
        });
        self
    }

    /// Validate every queued op, then log one `Batch` frame and apply.
    /// Empty / all-noop batches return `Ok(())` without writing the WAL.
    /// A second `commit()` after a successful one is an empty-batch no-op
    /// (queued ops were taken).
    /// Takes `&mut self` so it chains after the queue methods (`b.insert_node(..).commit()`)
    /// and also works as `let mut b = db.batch(); b.insert_node(..); b.commit()`.
    ///
    /// **Rule-window limitation:** batch validation cannot see edges that a
    /// rule created earlier in the *same* batch will derive at apply time, so
    /// a `delete_edge` / `insert_edge` in that window is silently no-oped
    /// where sequential calls would return `Err(RuleOwned)`. State integrity
    /// is unaffected (idempotent apply, provenance intact). Create rules in
    /// their own batch, or sequentially, when later ops may touch derived
    /// edges.
    /// Validate every queued op and commit atomically.
    ///
    /// Returns `(nodes_inserted, edges_inserted)` — the counts of node and edge
    /// WAL records actually written (duplicate edges are silent no-ops and are
    /// NOT counted). Both are 0 when the batch is empty or all-noop.
    pub fn commit(&mut self) -> Result<(usize, usize)> {
        let ops = std::mem::take(&mut self.ops);
        self.db.commit_batch(ops)
    }

    /// [`commit`](Self::commit) with the full [`BatchOutcome`] — the counts a
    /// caller needs when its rows carry an [`OnConflict`] policy.
    pub fn commit_outcome(&mut self) -> Result<BatchOutcome> {
        let ops = std::mem::take(&mut self.ops);
        self.db.commit_logged_batch(ops, None, None)
    }

    /// Same as [`commit`](Self::commit) but tail the inner events with
    /// [`MutationEvent::Ingested`] instead of [`MutationEvent::BatchApplied`].
    pub(crate) fn commit_ingest(&mut self, label: &str, inserted: usize) -> Result<(usize, usize)> {
        let ops = std::mem::take(&mut self.ops);
        self.db
            .commit_logged_batch(ops, Some((label.to_string(), inserted)), None)
            .map(inserted_pair)
    }
}

pub struct NodeRef<'a, F: Fs> {
    db: &'a GraphDb<F>,
    id: u32,
}

impl<'a, F: Fs> NodeRef<'a, F> {
    pub fn key(&self) -> &str {
        self.db.ids.key_of(self.id).expect("dense ids")
    }

    pub fn label(&self) -> &str {
        let sym = self
            .db
            .labels
            .get(self.id as usize)
            .copied()
            .filter(|&s| s != u32::MAX)
            .expect("real nodes always have a label; u32::MAX sentinel cannot occur");
        self.db.syms.resolve(sym).expect("interned label symbol")
    }

    pub fn prop(&self, field: &str) -> Option<Value> {
        self.db
            .props_view()
            .get(self.id, field)
            .map(|vr| vr.into_value())
    }

    /// All stored fields for this node, sorted by field name.
    ///
    /// Reads from the full base+overlay view so that props stored only in the
    /// V8 snapshot base (i.e. before any post-snapshot WAL writes) are visible.
    pub fn props(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        let pv = self.db.props_view();
        for field in pv.field_names() {
            if let Some(vr) = pv.get(self.id, &field) {
                out.insert(field, vr.into_value());
            }
        }
        out
    }

    /// depth-N BFS as a ResultSet: columns ["key","label","depth"], BFS order.
    pub fn neighborhood(&self, depth: u32, edge_types: Option<&[&str]>, dir: Dir) -> ResultSet {
        let view = self.db.view();
        let resolved: Option<Vec<u32>> = edge_types.map(|names| {
            names
                .iter()
                .filter_map(|name| view.syms.get(name))
                .collect()
        });
        let nb = neighborhood(&view, self.id, depth, resolved.as_deref(), dir);
        let mut rs = ResultSet::new(vec!["key".into(), "label".into(), "depth".into()]);
        for (nid, d) in nb.nodes {
            let key = view.key_of(nid);
            let label = view
                .label_of(nid)
                .expect("real nodes always have a label; u32::MAX sentinel cannot occur");
            rs.push_row(vec![
                Some(Value::Str(key.to_string())),
                Some(Value::Str(label.to_string())),
                Some(Value::Int(d as i64)),
            ]);
        }
        rs
    }

    /// 1-hop, Both directions: edge-type name → sorted unique neighbor keys.
    pub fn grouped_by_edge_type(&self) -> BTreeMap<String, Vec<String>> {
        let view = self.db.view();
        let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for e in expand(&view, self.id, None, Dir::Both) {
            // Skip edges with unknown etypes (only possible from corrupt large
            // TOPOLOGY section; function returns BTreeMap not Result).
            let Some(etype) = view.syms.resolve(e.etype) else {
                continue;
            };
            let etype = etype.to_string();
            let nbr = if e.src == self.id { e.dst } else { e.src };
            groups
                .entry(etype)
                .or_default()
                .insert(view.key_of(nbr).to_string());
        }
        groups
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect()
    }
}

/// A copy of a lock-attempt failure that a denied write scope can return more
/// than once. Only the two I/O-derived kinds are kept; `GraphError` is not
/// `Clone`, and an `io::Error` is rebuilt from its kind and text.
fn copy_lock_failure(err: &GraphError) -> Option<GraphError> {
    match err {
        GraphError::Unwritable {
            path,
            detail,
            read_only_filesystem,
        } => Some(GraphError::Unwritable {
            path: path.clone(),
            detail: detail.clone(),
            read_only_filesystem: *read_only_filesystem,
        }),
        GraphError::Io(e) => Some(GraphError::Io(std::io::Error::new(e.kind(), e.to_string()))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_rules::Predicate;

    /// Ledger row 126: a property write that derives nothing leaves the
    /// topology, edge properties and interner shared with the fold, on a
    /// store with no snapshot (every edge in the overlay) and with a rule
    /// installed, so the engine runs. A write that derives an edge copies the
    /// topology, and the fold keeps its own.
    #[test]
    fn a_write_that_derives_nothing_copies_no_topology() {
        let dir = tmp_dir("row126-topo");
        let mut db = GraphDb::open(&dir).unwrap();
        db.create_rule(fk_rule()).unwrap();
        let mut b = db.batch();
        for i in 0..50 {
            b.insert_node("Person", &format!("p{i}"), vec![]);
        }
        b.insert_node("Org", "acme", vec![]);
        for i in 0..49 {
            b.insert_edge("KNOWS", &format!("p{i}"), &format!("p{}", i + 1));
        }
        b.commit().unwrap();
        // The field's name is interned here, before the fold: a write that
        // names a new symbol writes the interner, and copies it.
        db.set_prop("p3", "age", Value::Int(6)).unwrap();
        db.fold_now();
        let fold = Arc::clone(db.fold_overlay.as_ref().unwrap());
        db.set_prop("p3", "age", Value::Int(7)).unwrap();
        assert!(Arc::ptr_eq(&db.topo, &fold.topo), "topology copied");
        assert!(
            Arc::ptr_eq(&db.edge_props, &fold.edge_props),
            "edge props copied"
        );
        assert!(Arc::ptr_eq(&db.syms, &fold.syms), "interner copied");
        // A write the rule derives from: the writer copies, the fold does not move.
        db.set_prop("p4", "org_id", Value::Str("acme".into()))
            .unwrap();
        assert!(!Arc::ptr_eq(&db.topo, &fold.topo));
        assert_eq!(
            db.neighbors("p4", "WORKS_AT", Direction::Out).unwrap(),
            ["acme"]
        );
        let org = fold.ids.get("acme").unwrap();
        let p4 = fold.ids.get("p4").unwrap();
        let et = fold.syms.get("WORKS_AT");
        assert!(et.is_none_or(|et| !fold.topo.neighbors(et, Direction::Out, p4).contains(&org)));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ledger row 130: a node delete in a batch finds the batch's edges at
    /// that node through the endpoint index. 201 deletes, each looking in the
    /// added and the deleted edge sets, read at most the edges at the node and
    /// the one entry that ends each walk (403 here), not the 200 × 200 a full
    /// walk reads (20,300 with the old `retain`).
    #[test]
    fn a_batch_node_delete_visits_only_the_batch_edges_at_that_node() {
        let dir = tmp_dir("row130-visits");
        let mut db = GraphDb::open(&dir).unwrap();
        let mut b = db.batch();
        for i in 0..200 {
            b.insert_node("Old", &format!("old{i}"), vec![]);
        }
        b.commit().unwrap();
        let mut b = db.batch();
        for i in 0..200 {
            b.insert_node("P", &format!("s{i}"), vec![])
                .insert_node("P", &format!("d{i}"), vec![])
                .insert_edge("KNOWS", &format!("s{i}"), &format!("d{i}"))
                .delete_node(&format!("old{i}"));
        }
        b.delete_node("s0");
        EDGE_KEYS_VISITED.with(|v| v.set(0));
        b.commit().unwrap();
        let visited = EDGE_KEYS_VISITED.with(|v| v.get());
        assert!(visited <= 2 * 201 + 1, "{visited} entries read");
        assert_eq!(db.edge_count(), 199);
        assert!(db.node_info("s0").is_none() && db.node_info("old7").is_none());
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("graphdb-db-unit-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn fk_rule() -> RuleDef {
        RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
            namespace: None,
        }
    }

    /// Regression guard for the no-views delta-copy fast path.
    ///
    /// When no views are defined, `pending_deltas_since().to_vec()` must never
    /// be called — even during a large CreateRule backfill. The DELTA_COPY_COUNT
    /// thread-local is incremented inside every `if !view_store.is_empty()` block;
    /// a count of 0 after the entire sequence proves the guard fires correctly.
    #[test]
    fn no_delta_copy_when_no_views() {
        DELTA_COPY_COUNT.with(|c| c.set(0));
        let dir = tmp_dir("no-delta-copy");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            // Insert 50 Org + 50 Person nodes with FK links.
            for i in 0..50u32 {
                db.insert_node("Org", &format!("o{i}"), vec![]).unwrap();
            }
            for i in 0..50u32 {
                db.insert_node(
                    "Person",
                    &format!("p{i}"),
                    vec![("org_id".into(), Value::Str(format!("o{i}")))],
                )
                .unwrap();
            }
            // CreateRule backfill should NOT invoke to_vec() when no views are defined.
            db.create_rule(fk_rule()).unwrap();

            // Counter must stay 0 — no views, no copies.
            let copies = DELTA_COPY_COUNT.with(|c| c.get());
            assert_eq!(
                copies, 0,
                "pending_deltas_since().to_vec() called despite no views"
            );

            // Derived edges must still be correct (the guard skips only the
            // empty delta propagation loop, not the rule application itself).
            let nbrs = db.neighbors("p0", "WORKS_AT", Direction::Out).unwrap();
            assert_eq!(
                nbrs,
                vec!["o0"],
                "rule must derive edges even with no views"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Gating regression: subscribe AFTER a backfill must see no stale events.
    /// subscribe BEFORE a backfill must see every edge-fire event.
    #[test]
    fn subscribe_after_backfill_no_stale_events() {
        let dir = tmp_dir("sub-after-backfill");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            for i in 0..10u32 {
                db.insert_node("Org", &format!("o{i}"), vec![]).unwrap();
                db.insert_node(
                    "Person",
                    &format!("p{i}"),
                    vec![("org_id".into(), Value::Str(format!("o{i}")))],
                )
                .unwrap();
            }
            // Create rule BEFORE subscribing — emit_deltas is false during backfill.
            db.create_rule(fk_rule()).unwrap();

            // Subscribe AFTER the backfill — queue must be empty (no stale events).
            let sub = db.subscribe_all_rules().unwrap();
            // No events should have queued for the prior backfill.
            assert!(
                sub.try_recv().is_none(),
                "subscribe after backfill must see no stale events"
            );

            // Inserting a new node now should fire an event (emit_deltas is now true).
            db.insert_node("Org", "o_new", vec![]).unwrap();
            db.insert_node(
                "Person",
                "p_new",
                vec![("org_id".into(), Value::Str("o_new".into()))],
            )
            .unwrap();
            let ev = sub.recv_timeout(std::time::Duration::from_millis(200));
            assert!(
                ev.is_some(),
                "edge-fire event must arrive after subscribe (emit_deltas=true)"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Gating regression: subscribe BEFORE a backfill → events flow.
    #[test]
    fn subscribe_before_backfill_events_flow() {
        let dir = tmp_dir("sub-before-backfill");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            // Subscribe FIRST — emit_deltas becomes true.
            let sub = db.subscribe_all_rules().unwrap();

            for i in 0..5u32 {
                db.insert_node("Org", &format!("o{i}"), vec![]).unwrap();
                db.insert_node(
                    "Person",
                    &format!("p{i}"),
                    vec![("org_id".into(), Value::Str(format!("o{i}")))],
                )
                .unwrap();
            }
            // Backfill fires with emit_deltas=true → events queued.
            db.create_rule(fk_rule()).unwrap();

            // Should receive at least one edge-fired event from the backfill.
            let mut received = 0usize;
            while sub.try_recv().is_some() {
                received += 1;
            }
            assert!(
                received > 0,
                "subscribe before backfill must receive edge-fire events (got 0)"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Companion: when a view IS defined, the delta path fires and view values update.
    #[test]
    fn delta_copy_fires_when_view_exists() {
        use core_rules::ViewSource;
        DELTA_COPY_COUNT.with(|c| c.set(0));
        let dir = tmp_dir("delta-copy-with-view");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            db.insert_node("Org", "o1", vec![]).unwrap();
            db.insert_node(
                "Person",
                "p1",
                vec![("org_id".into(), Value::Str("o1".into()))],
            )
            .unwrap();
            // Declare a Degree view so is_empty() returns false.
            db.create_view(ViewDef {
                name: "degree_out".into(),
                label: "Person".into(),
                view_prop: "degree_out".into(),
                source: ViewSource::Degree {
                    edge_type: "WORKS_AT".into(),
                    direction: Direction::Out,
                },
            })
            .unwrap();
            db.create_rule(fk_rule()).unwrap();

            // At least one delta copy should have happened (CreateRule backfill).
            let copies = DELTA_COPY_COUNT.with(|c| c.get());
            assert!(
                copies > 0,
                "expected delta copy to fire when a view is defined"
            );

            // View value should be computed: p1 has one WORKS_AT out-edge.
            let info = db.node_info("p1").unwrap();
            let degree = info.props.get("degree_out");
            assert!(
                degree.is_some(),
                "view prop should be written to node props"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: `open_at_with` must call `rebuild_all` after WAL replay so
    /// derived-edge-driven view values reflect the as-of state rather than just
    /// the initial backfill written at `CreateView` time.
    ///
    /// Base WAL frames (indices 0..=5 before history markers):
    ///   0: insert Org "o1"
    ///   1: create_view "employee_count" (Degree / WORKS_AT / In) on Org
    ///   2: create_rule fk_rule (WORKS_AT, Person→Org via org_id)
    ///   3: insert Person "p1" → rule fires WORKS_AT p1→o1 (degree = 1)  ← mid
    ///   4: insert Person "p2" → rule fires WORKS_AT p2→o1 (degree = 2)
    ///   5: insert Person "p3" → rule fires WORKS_AT p3→o1 (degree = 3)  ← latest
    ///
    /// Each rule-fire also appends a DerivedEdgeAdded history-marker frame (state
    /// no-op), so the total commit count is higher than the base frame count.
    /// The "latest" open_at commit is computed dynamically via `wal_commit_count_at`.
    ///
    /// Without `rebuild_all`, the as-of instance's "emp" view stays at the
    /// initial backfill value (0) instead of reflecting the replayed derived edges.
    #[test]
    fn open_at_derived_edge_view_values_correct() {
        use core_rules::ViewSource;
        let dir = tmp_dir("open-at-view-rebuild");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            // frame 0
            db.insert_node("Org", "o1", vec![]).unwrap();
            // frame 1: create view — initial backfill sees 0 derived edges (none fired yet)
            db.create_view(ViewDef {
                name: "employee_count".into(),
                label: "Org".into(),
                view_prop: "emp".into(),
                source: ViewSource::Degree {
                    edge_type: "WORKS_AT".into(),
                    direction: Direction::In,
                },
            })
            .unwrap();
            // frame 2: create rule — no Persons yet; backfill is a no-op
            db.create_rule(fk_rule()).unwrap();
            // frame 3: p1 — rule fires WORKS_AT p1→o1; degree = 1
            db.insert_node(
                "Person",
                "p1",
                vec![("org_id".into(), Value::Str("o1".into()))],
            )
            .unwrap();
            // frame 4: p2 — degree = 2
            db.insert_node(
                "Person",
                "p2",
                vec![("org_id".into(), Value::Str("o1".into()))],
            )
            .unwrap();
            // frame 5: p3 — degree = 3
            db.insert_node(
                "Person",
                "p3",
                vec![("org_id".into(), Value::Str("o1".into()))],
            )
            .unwrap();
            // Sanity: normal open sees degree = 3.
            assert_eq!(
                db.get_view_prop("o1", "emp"),
                Some(Value::Int(3)),
                "normal db must show degree 3 after 3 derived edges"
            );
        } // WAL flushed

        // Re-open normally to get the authoritative reference value.
        let normal_db = GraphDb::open(&dir).unwrap();
        let normal_emp = normal_db.get_view_prop("o1", "emp");
        assert_eq!(
            normal_emp,
            Some(Value::Int(3)),
            "re-opened normal db must show degree 3"
        );

        // Latest as-of (last WAL commit): must match the normal open.
        // History-marker frames are appended after each rule-fire, so the total
        // commit count is computed dynamically rather than hardcoded.
        let total = crate::wal_commit_count_at(&dir).unwrap();
        let aof_latest = GraphDb::open_at(&dir, total - 1).unwrap();
        assert_eq!(
            aof_latest.get_view_prop("o1", "emp"),
            normal_emp,
            "open_at latest: derived-edge view must equal normal open (rebuild_all required)"
        );

        // Mid-history as-of (commit 3 = p1 insert Batch frame): only p1; degree = 1.
        // The DerivedEdgeAdded marker for p1 is at frame 4 (state no-op on replay),
        // so replaying 0..=3 correctly re-derives only the p1→o1 edge.
        let aof_mid = GraphDb::open_at(&dir, 3).unwrap();
        assert_eq!(
            aof_mid.get_view_prop("o1", "emp"),
            Some(Value::Int(1)),
            "open_at mid-history: only p1 exists at frame 3, degree must be 1"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pin: subscribe_* on an as-of instance must return Err(ReadOnly) —
    /// as-of instances never commit, so distribute_events never runs and any
    /// subscription would wait forever.
    #[test]
    fn subscribe_on_as_of_returns_read_only_error() {
        let dir = tmp_dir("sub-as-of-read-only");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            db.insert_node("Org", "o1", vec![]).unwrap();
            db.create_rule(fk_rule()).unwrap();
        }
        let mut aof = GraphDb::open_at(&dir, 0).unwrap();

        assert!(
            matches!(
                aof.subscribe_all_rules(),
                Err(core_storage::GraphError::ReadOnly)
            ),
            "subscribe_all_rules on as-of must return ReadOnly"
        );
        assert!(
            matches!(
                aof.subscribe_writes(),
                Err(core_storage::GraphError::ReadOnly)
            ),
            "subscribe_writes on as-of must return ReadOnly"
        );
        assert!(
            matches!(
                aof.subscribe_rule("works_at"),
                Err(core_storage::GraphError::ReadOnly)
            ),
            "subscribe_rule on as-of must return ReadOnly"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: a failed dense WAL rewrite must not leave speculative
    /// interns in `syms`. If it does, the next successful mutation logs an
    /// `Intern` record with an inflated id; replay (which never saw the
    /// orphans) assigns a smaller id and the WAL becomes unreplayable.
    #[test]
    fn dense_rewrite_error_rolls_back_speculative_interns() {
        let dir = tmp_dir("dense-rewrite-rollback");
        {
            let mut db = GraphDb::open(&dir).unwrap();
            db.insert_node("Person", "a", vec![]).unwrap();

            // Bypass MutPreview validation to hit the rewrite's own error path
            // (same shape as an id-exhaustion failure mid-rewrite). The
            // InsertEdge arm interns the edge type before it resolves keys.
            let err = db.rewrite_wal_dense(vec![WalRecord::InsertEdge {
                edge_type: "ORPHAN_TYPE".into(),
                src_key: "missing".into(),
                dst_key: "a".into(),
            }]);
            assert!(err.is_err(), "rewrite of a missing src key must fail");
            assert_eq!(
                db.syms.get("ORPHAN_TYPE"),
                None,
                "failed rewrite must roll back speculative interns"
            );

            // A later successful mutation must produce a replayable WAL.
            db.set_prop("a", "later_field", Value::Int(2)).unwrap();
        }
        let db = GraphDb::open(&dir).expect("WAL must replay after failed rewrite");
        assert_eq!(db.get_prop("a", "later_field"), Some(Value::Int(2)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

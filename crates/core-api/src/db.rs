use crate::ingest::{IngestOptions, IngestReport};
use core_query::cypher::{
    execute, lex, parse, parse_write, plan, MatchDeleteNodeStmt, Params, Query, RetItem, RetVal,
    WriteStatement,
};
use core_query::{eval_filter, expand, neighborhood, Dir, Filter, GraphView, ResultSet};
use core_rules::{
    evaluate, EngineEdgeDelta, GraphMut, NodeView, Predicate, RuleDef, RuleEngine, RuleIvfExport,
};
use crate::subscription::{
    event_matches, DbEvent, SubEntry, SubFilter, SubInner, Subscription, DEFAULT_SUB_CAPACITY,
};
use core_storage::fs::{FileId, Fs, FsIntrospect, RealFs};
use core_storage::wal::{decode_all, encode_record, WalRecord};
use core_storage::{
    ColumnStore, Direction, EdgeProps, GraphError, IdMap, Interner, Result, Topology, Value,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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

fn event_from_record(rec: &WalRecord) -> Option<MutationEvent> {
    match rec {
        WalRecord::InsertNode { label, key, .. } => Some(MutationEvent::NodeInserted {
            label: label.clone(),
            key: key.clone(),
        }),
        WalRecord::SetProp { key, field, .. } => Some(MutationEvent::PropSet {
            key: key.clone(),
            field: field.clone(),
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
            let def: RuleDef = bincode::deserialize(def_bytes).ok()?;
            Some(MutationEvent::RuleCreated { name: def.name })
        }
        WalRecord::DeleteRule { name } => Some(MutationEvent::RuleDeleted { name: name.clone() }),
        WalRecord::RebuildRule { name } => Some(MutationEvent::RuleRebuilt { name: name.clone() }),
        WalRecord::Batch(_) => None,
    }
}

/// Database-wide counters plus per-rule budget/fire stats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub nodes_live: usize,
    pub nodes_tombstoned: usize,
    pub edges: u64,
    pub rules: Vec<RuleStats>,
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
}

/// Construct the standard write-query result set (columns: created, properties_set, deleted).
fn write_result_set() -> ResultSet {
    ResultSet::new(vec![
        "created".into(),
        "properties_set".into(),
        "deleted".into(),
    ])
}

/// Single construction point for a `GraphMut` view over the split-borrowed graph fields.
/// Callers use `std::mem::take` on the engine before calling this, then restore it after.
fn make_graph_mut<'a>(
    ids: &'a IdMap,
    syms: &'a mut Interner,
    labels: &'a [u32],
    props: &'a ColumnStore,
    topo: &'a mut Topology,
    edge_props: &'a mut EdgeProps,
) -> GraphMut<'a> {
    GraphMut {
        ids,
        syms,
        labels,
        props,
        topo,
        edge_props,
    }
}

pub struct GraphDb<F: Fs> {
    fs: F,
    ids: IdMap,
    syms: Interner,
    topo: Topology,
    props: ColumnStore,
    labels: Vec<u32>, // node id -> label symbol
    edge_props: EdgeProps,
    engine: RuleEngine,
    event_sink: Option<Box<dyn Fn(MutationEvent) + Send + Sync>>,
    /// Monotonically increasing per-commit counter.  A single `log_then_apply_with`
    /// call increments this once; all events emitted from that call share the same
    /// `commit_seq` value.
    commit_seq: u64,
    /// Live subscriptions.  Entries with a dead `Weak` are pruned on the next
    /// distribute_events call.
    subscriptions: Vec<SubEntry>,
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
}

impl GraphDb<RealFs> {
    pub fn open(dir: &std::path::Path) -> Result<Self> {
        Self::open_with(RealFs::new(dir)?)
    }

    /// Open a read-only view of the database as it existed after `commit`.
    ///
    /// Commit indices are 0-based over the current WAL: commit 0 is the state
    /// after the first WAL frame, commit N-1 is the state after the N-th (most
    /// recent) frame.  Call [`GraphDb::open`] to read the full current state.
    ///
    /// **WAL-only replay.** The snapshot file (if any) is ignored.  mushroomdb's
    /// [`GraphDb::snapshot`] truncates the WAL to empty when it runs, so
    /// as-of can only reach commits recorded in the current WAL (those written
    /// after the most recent snapshot, or all commits if no snapshot was ever
    /// taken).  Commit 0 in `open_at` always refers to the first frame in the
    /// WAL that exists on disk, not the first ever write to the database.
    ///
    /// **Read-only.** Every mutation method and `snapshot()` on the returned
    /// instance returns [`GraphError::ReadOnly`].  Queries, `explain()`, and
    /// `stats()` work normally.
    ///
    /// # Errors
    /// - [`GraphError::CommitOutOfRange`] if `commit >= wal_commit_count` (including
    ///   when the WAL is empty after a snapshot).
    pub fn open_at(dir: &std::path::Path, commit: u64) -> Result<Self> {
        Self::open_at_with(RealFs::new(dir)?, commit)
    }
}

impl<F: Fs> GraphDb<F> {
    pub fn open_with(fs: F) -> Result<Self> {
        let mut db = Self {
            fs,
            ids: IdMap::new(),
            syms: Interner::new(),
            topo: Topology::new(),
            props: ColumnStore::new(),
            labels: Vec::new(),
            edge_props: EdgeProps::new(),
            engine: RuleEngine::new(),
            event_sink: None,
            commit_seq: 0,
            subscriptions: Vec::new(),
            sub_capacity: DEFAULT_SUB_CAPACITY,
            read_only: false,
            total_wal_commits: 0,
        };
        let snap_bytes = db.fs.read(FileId::Snapshot)?;
        if let Some(state) = core_storage::snapshot::decode(&snap_bytes)? {
            db.ids = state.ids;
            db.syms = state.syms;
            db.topo = state.topo;
            db.props = state.props;
            db.labels = state.labels;
            db.edge_props = state.edge_props;
            let defs: Vec<RuleDef> = state
                .rule_defs
                .iter()
                .map(|b| {
                    bincode::deserialize(b).map_err(|e| GraphError::Corrupt {
                        detail: format!("snapshot rule_def deserialize: {e}"),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            db.engine = RuleEngine::from_persist(
                defs,
                state.provenance,
                state.rule_tripped,
                state.rule_fires,
            );
            // V4 snapshot carries IVF state: restore it instead of re-fitting.
            // This turns the cold-start multi-minute re-fit into microseconds.
            let ivf_state: BTreeMap<String, RuleIvfExport> = state
                .ivf_state
                .into_iter()
                .map(|(name, ps)| {
                    (
                        name,
                        (
                            (ps.src.centroids, ps.src.clusters, ps.src.drift),
                            (ps.dst.centroids, ps.dst.clusters, ps.dst.drift),
                        ),
                    )
                })
                .collect();
            db.engine.reindex_all_load_ivf(
                &db.ids,
                &db.syms,
                &db.labels,
                &db.props,
                ivf_state,
            );
        }
        let bytes = db.fs.read(FileId::Wal)?;
        let (records, valid_len) = decode_all(&bytes);
        if valid_len < bytes.len() {
            db.fs.write_atomic(FileId::Wal, &bytes[..valid_len])?;
        }
        for rec in records {
            db.apply(&rec)?;
            // Drain per-frame to keep pending_deltas O(1) during replay (I-2).
            // No subscriber exists yet; discard is correct.
            let _ = db.engine.drain_deltas();
        }
        // Enforce I-2: if the per-frame drain above is ever removed or skipped,
        // this assert catches the regression in debug builds immediately.
        debug_assert_eq!(
            db.engine.pending_delta_count(),
            0,
            "pending_deltas non-empty after replay — \
             per-frame drain must run inside the loop to keep memory O(1)"
        );
        // T2 note: the per-frame drain IS the suppression seam for replay.
        // Any future as-of replay path (Plan-15 T2) must drain here to feed
        // replaying subscribers; the mechanism is already in place.
        let _ = db.engine.drain_deltas(); // belt-and-braces no-op after loop drain
        Ok(db)
    }

    /// WAL-only as-of replay for [`GraphDb::open_at`].
    ///
    /// Snapshot is deliberately not loaded; see [`GraphDb::open_at`] for the
    /// design rationale.  The per-frame drain mirrors `open_with` exactly so
    /// pending_delta_count is 0 on exit.
    fn open_at_with(fs: F, commit: u64) -> Result<Self> {
        let mut db = Self {
            fs,
            ids: IdMap::new(),
            syms: Interner::new(),
            topo: Topology::new(),
            props: ColumnStore::new(),
            labels: Vec::new(),
            edge_props: EdgeProps::new(),
            engine: RuleEngine::new(),
            event_sink: None,
            commit_seq: 0,
            subscriptions: Vec::new(),
            sub_capacity: DEFAULT_SUB_CAPACITY,
            read_only: false, // set to true after replay
            total_wal_commits: 0,
        };
        let bytes = db.fs.read(FileId::Wal)?;
        let (records, _valid_len) = decode_all(&bytes);
        let total = records.len() as u64;
        if commit >= total {
            return Err(GraphError::CommitOutOfRange { commit, total });
        }
        // Replay frames 0..=commit — identical drain pattern to open_with so
        // the pending_delta_count == 0 invariant holds.
        for rec in records.into_iter().take((commit + 1) as usize) {
            db.apply(&rec)?;
            // Drain per-frame: no subscriber exists; discard is correct.
            // This keeps memory O(1) and mirrors the open_with seam exactly.
            let _ = db.engine.drain_deltas();
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
        db.read_only = true;
        db.total_wal_commits = total;
        Ok(db)
    }

    /// Whether this instance is a read-only as-of view.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Total number of WAL commits at the time [`open_at`] was called.
    /// Returns 0 for normal (non-as-of) instances.
    pub fn total_wal_commits(&self) -> u64 {
        self.total_wal_commits
    }

    /// Apply a record to in-memory state. Used by both live writes and replay,
    /// so replay is definitionally identical to the original execution.
    fn apply(&mut self, rec: &WalRecord) -> Result<()> {
        match rec {
            WalRecord::InsertNode { label, key, props } => {
                let id = self.ids.get_or_insert(key);
                let sym = self.syms.intern(label);
                if self.labels.len() <= id as usize {
                    // gap slots are sentinels, never valid label symbols
                    self.labels.resize(id as usize + 1, u32::MAX);
                }
                self.labels[id as usize] = sym;
                for (field, value) in props {
                    self.props.set(id, field, value.clone());
                }
                // Fire rules for the newly inserted node.
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, None, &mut gm);
                }
                self.engine = eng;
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
                let etype = self.syms.intern(edge_type);
                self.topo.add_edge(etype, src, dst);
            }
            WalRecord::SetProp { key, field, value } => {
                let id = self.ids.get(key).ok_or_else(|| GraphError::Corrupt {
                    detail: format!("wal replay references unknown key {key}"),
                })?;
                let old_value = self.props.get(id, field).cloned();
                self.props.set(id, field, value.clone());
                // Fire rules for the changed field.
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, Some((field, old_value)), &mut gm);
                }
                self.engine = eng;
            }
            WalRecord::CreateRule { def_bytes } => {
                let def: RuleDef =
                    bincode::deserialize(def_bytes).map_err(|e| GraphError::Corrupt {
                        detail: format!("CreateRule def_bytes deserialize failed: {e}"),
                    })?;
                // Replay-over-snapshot idempotency: the rule was captured in the snapshot
                // so the engine already has it; silently skip to avoid a spurious
                // RuleInvalid error in the crash window between snapshot write and WAL
                // truncation.
                if self.engine.rules().any(|r| r.name == def.name) {
                    return Ok(());
                }
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.create_rule(def, &mut gm)
                };
                self.engine = eng;
                result.map_err(|e| GraphError::RuleInvalid { detail: e })?;
            }
            WalRecord::DeleteRule { name } => {
                // Replay-over-snapshot idempotency: the snapshot already captured the
                // post-delete state so the rule is absent; silently skip to avoid a
                // spurious RuleNotFound error in the crash window between snapshot write
                // and WAL truncation.
                if !self.engine.rules().any(|r| r.name == *name) {
                    return Ok(());
                }
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.delete_rule(name, &mut gm)
                };
                self.engine = eng;
                result.map_err(|_| GraphError::RuleNotFound { name: name.clone() })?;
            }
            WalRecord::RemoveProp { key, field } => {
                // Recovery-safe: unknown key or already-absent field is a
                // clean no-op. Crash-window replay over a snapshot that
                // already applied this record must not Err.
                let Some(id) = self.ids.get(key) else {
                    return Ok(());
                };
                let old = self.props.get(id, field).cloned();
                self.props.remove(id, field);
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_changed(id, Some((field, old)), &mut gm);
                }
                self.engine = eng;
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
                self.topo.remove_edge(etype, src, dst);
                self.edge_props.remove_edge(etype, src, dst);
                // No rule callback: validated as not provenance-owned and not
                // would_derive, so no rule needs to update its desired set.
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
                let mut eng = std::mem::take(&mut self.engine);
                {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.on_node_removed(n, &mut gm);
                }
                self.engine = eng;

                // (2) Sweep remaining user edges touching n, both directions,
                // every etype. Collect then remove so neighbor slices stay valid.
                let etypes: Vec<u32> = self.topo.etypes().collect();
                let mut doomed = Vec::new();
                for et in etypes {
                    for &dst in self.topo.neighbors(et, Direction::Out, n) {
                        doomed.push((et, n, dst));
                    }
                    for &src in self.topo.neighbors(et, Direction::In, n) {
                        doomed.push((et, src, n));
                    }
                }
                for (et, s, d) in doomed {
                    self.topo.remove_edge(et, s, d);
                    self.edge_props.remove_edge(et, s, d);
                }

                // (3) Drop every remaining prop (`ColumnStore::remove_all`).
                self.props.remove_all(n);

                // (4) Retire the dense id and stamp the label sentinel.
                self.ids.delete(key);
                if let Some(slot) = self.labels.get_mut(n as usize) {
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
                let mut eng = std::mem::take(&mut self.engine);
                let result = {
                    let mut gm = make_graph_mut(
                        &self.ids,
                        &mut self.syms,
                        &self.labels,
                        &self.props,
                        &mut self.topo,
                        &mut self.edge_props,
                    );
                    eng.rebuild(name, &mut gm)
                };
                self.engine = eng;
                result.map_err(|_| GraphError::RuleNotFound { name: name.clone() })?;
            }
        }
        Ok(())
    }

    /// Durable write, then notify the event sink. Replay (`apply` during
    /// `open`) never enters this function, so it is the replay-silent seam.
    fn log_then_apply(&mut self, rec: WalRecord) -> Result<()> {
        self.log_then_apply_with(rec, None)
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
    ) -> Result<()> {
        // Read-only guard: as-of instances must never write the WAL.
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
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
        self.fs.append(FileId::Wal, &encode_record(&rec))?;
        self.fs.sync(FileId::Wal)?; // strict policy in plan 1
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
            let _ = self.engine.drain_deltas();
            apply_result?;
        }
        self.commit_seq += 1;
        let seq = self.commit_seq;
        // Drain engine deltas and distribute to subscribers before the existing
        // MutationEvent sink fires — both happen post-fsync, post-apply.
        let engine_deltas = self.engine.drain_deltas();
        self.distribute_events(&rec, &engine_deltas, seq);
        self.emit_committed(&rec, ingest);
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

    fn emit(&self, ev: MutationEvent) {
        if let Some(sink) = &self.event_sink {
            sink(ev);
        }
    }

    fn emit_committed(&self, rec: &WalRecord, ingest: Option<(String, usize)>) {
        match rec {
            WalRecord::Batch(inner) => {
                for r in inner {
                    if let Some(ev) = event_from_record(r) {
                        self.emit(ev);
                    }
                }
                match ingest {
                    Some((label, inserted)) => {
                        self.emit(MutationEvent::Ingested { label, inserted })
                    }
                    None => self.emit(MutationEvent::BatchApplied { ops: inner.len() }),
                }
            }
            other => {
                if let Some(ev) = event_from_record(other) {
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
    /// Called from `log_then_apply_with` after apply + fsync, before the
    /// legacy MutationEvent sink. Prunes dead `Weak` entries in-place.
    fn distribute_events(
        &mut self,
        rec: &WalRecord,
        engine_deltas: &[EngineEdgeDelta],
        seq: u64,
    ) {
        if self.subscriptions.is_empty() {
            return;
        }

        // Build write events from the WAL record.
        let write_events: Vec<DbEvent> = Self::write_events_from_record(rec, seq);

        // Build edge events from engine deltas.  Weight is looked up from
        // edge_props at distribution time (after apply), so it's always fresh.
        let edge_events: Vec<DbEvent> = engine_deltas
            .iter()
            .map(|d| {
                if d.fired {
                    let weight = self
                        .edge_props
                        .get(d.etype_sym, d.src_id, d.dst_id, "weight")
                        .and_then(|v| {
                            if let core_storage::Value::Float(f) = v {
                                Some(*f)
                            } else {
                                None
                            }
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
    }

    /// Convert a WAL record into `DbEvent` write events with the given seq.
    fn write_events_from_record(rec: &WalRecord, seq: u64) -> Vec<DbEvent> {
        match rec {
            WalRecord::InsertNode { label, key, .. } => vec![DbEvent::NodeInserted {
                label: label.clone(),
                key: key.clone(),
                commit_seq: seq,
            }],
            WalRecord::SetProp { key, field, .. } => vec![DbEvent::PropSet {
                key: key.clone(),
                field: field.clone(),
                commit_seq: seq,
            }],
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
                .flat_map(|r| Self::write_events_from_record(r, seq))
                .collect(),
            WalRecord::CreateRule { .. }
            | WalRecord::DeleteRule { .. }
            | WalRecord::RebuildRule { .. } => vec![],
        }
    }

    /// Subscribe to edge-fire and edge-retract events for one named rule.
    ///
    /// Returns `Err(GraphError::RuleNotFound)` if `rule_name` is not
    /// currently registered. Dropping the returned [`Subscription`] handle
    /// unregisters the subscriber — no further events are queued, no
    /// resources leak.
    pub fn subscribe_rule(&mut self, rule_name: &str) -> core_storage::Result<Subscription> {
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
        Ok(Subscription(inner))
    }

    /// Subscribe to edge-fire and edge-retract events for **all** rules.
    pub fn subscribe_all_rules(&mut self) -> Subscription {
        let inner = SubInner::new(self.sub_capacity());
        self.subscriptions.push(SubEntry {
            filter: SubFilter::AllRules,
            inner: std::sync::Arc::downgrade(&inner),
        });
        Subscription(inner)
    }

    /// Subscribe to write events: node insert/delete, prop set/remove.
    ///
    /// Does not include edge-fire / edge-retract (rule-derived edge events).
    pub fn subscribe_writes(&mut self) -> Subscription {
        let inner = SubInner::new(self.sub_capacity());
        self.subscriptions.push(SubEntry {
            filter: SubFilter::Writes,
            inner: std::sync::Arc::downgrade(&inner),
        });
        Subscription(inner)
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
    ) -> Result<(usize, usize)> {
        // Read-only guard: catches empty-batch calls before the early-return
        // that skips log_then_apply_with, ensuring all mutation entry points fail.
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        let recs = {
            let mut preview = MutPreview::new(self);
            let mut recs = Vec::with_capacity(ops.len());
            for op in ops {
                match op {
                    BatchOp::InsertNode { label, key, props } => {
                        preview.check_insert_node(&key)?;
                        preview.note_insert_node(&key, &props);
                        recs.push(WalRecord::InsertNode { label, key, props });
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
                        }
                    }
                    BatchOp::SetProp { key, field, value } => {
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
                        preview.note_create_rule(&def.name);
                        recs.push(WalRecord::CreateRule { def_bytes });
                    }
                    BatchOp::DeleteRule { name } => {
                        preview.check_delete_rule(&name)?;
                        preview.note_delete_rule(&name);
                        recs.push(WalRecord::DeleteRule { name });
                    }
                }
            }
            recs
        };
        if recs.is_empty() {
            return Ok((0, 0));
        }
        let nodes_inserted = recs
            .iter()
            .filter(|r| matches!(r, WalRecord::InsertNode { .. }))
            .count();
        let edges_inserted = recs
            .iter()
            .filter(|r| matches!(r, WalRecord::InsertEdge { .. }))
            .count();
        self.log_then_apply_with(WalRecord::Batch(recs), ingest)?;
        Ok((nodes_inserted, edges_inserted))
    }

    fn commit_batch(&mut self, ops: Vec<BatchOp>) -> Result<(usize, usize)> {
        self.commit_logged_batch(ops, None)
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
        MutPreview::new(self).check_insert_node(key)?;
        self.log_then_apply(WalRecord::InsertNode {
            label: label.into(),
            key: key.into(),
            props,
        })
    }

    pub fn insert_edge(&mut self, edge_type: &str, src_key: &str, dst_key: &str) -> Result<bool> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if !MutPreview::new(self).prepare_insert_edge(edge_type, src_key, dst_key)? {
            return Ok(false);
        }
        self.log_then_apply(WalRecord::InsertEdge {
            edge_type: edge_type.into(),
            src_key: src_key.into(),
            dst_key: dst_key.into(),
        })?;
        Ok(true)
    }

    pub fn set_prop(&mut self, key: &str, field: &str, value: Value) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        MutPreview::new(self).check_live_key(key)?;
        self.log_then_apply(WalRecord::SetProp {
            key: key.into(),
            field: field.into(),
            value,
        })
    }

    /// Remove a property. Returns `Ok(false)` (and does not log) if the field
    /// is already absent. Unknown or tombstoned keys are `Err(KeyNotFound)`.
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
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;

        // Count edges before the delete is applied so we can report counts.
        let derived_set: BTreeSet<(u32, u32, u32)> = self
            .engine
            .provenance_touching(id)
            .map(|(_, etype, src, dst)| (etype, src, dst))
            .collect();
        let derived_edges = derived_set.len() as u64;

        let mut total_topo = 0u64;
        for et in self.topo.etypes() {
            total_topo +=
                self.topo.neighbors(et, Direction::Out, id).len() as u64
                    + self.topo.neighbors(et, Direction::In, id).len() as u64;
        }
        // For symmetric rules (e.g. Overlap), a→b and b→a are two separate directed
        // triples in both the topo scan (Out and In from id) and in provenance_touching.
        // The subtraction remains correct because both counts include both directions.
        let manual_edges = total_topo.saturating_sub(derived_edges);

        self.log_then_apply(WalRecord::DeleteNode { key: key.into() })?;
        Ok(DeleteReport {
            manual_edges,
            derived_edges,
        })
    }

    /// Return the IVF drift counter for the dst-side candidate index of `rule`.
    /// `None` if the rule does not exist or is not approximate.
    ///
    /// The drift counter increments whenever a node is removed from the IVF index
    /// (via `delete_node` or `remove_prop` on the vector field).  When drift exceeds
    /// a threshold, callers may trigger `rebuild_rule` to re-fit cluster centroids.
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
        self.log_then_apply(WalRecord::CreateRule { def_bytes })
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

    pub fn get_prop(&self, key: &str, field: &str) -> Option<&Value> {
        self.props.get(self.ids.get(key)?, field)
    }

    pub fn has_node(&self, key: &str) -> bool {
        self.ids.get(key).is_some()
    }

    fn view(&self) -> GraphView<'_> {
        GraphView {
            ids: &self.ids,
            syms: &self.syms,
            labels: &self.labels,
            props: &self.props,
            topo: &self.topo,
            edge_props: &self.edge_props,
        }
    }

    pub fn node_ref(&self, key: &str) -> Option<NodeRef<'_, F>> {
        let id = self.ids.get(key)?;
        Some(NodeRef { db: self, id })
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

    /// Every directed edge incident on `key`, both directions, every etype.
    ///
    /// Walk is `topology.etypes()` × `{Out, In}` × `neighbors()`. `derived` is
    /// membership in [`RuleEngine::provenance_touching`] (O(degree) via the
    /// Plan-8 `by_node` index). Sorted by `(edge_type, src_key, dst_key)`.
    /// Unknown key → [`GraphError::KeyNotFound`].
    pub fn node_edges(&self, key: &str) -> Result<Vec<EdgeInfo>> {
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
        for etype in self.topo.etypes() {
            let edge_type = self
                .syms
                .resolve(etype)
                .expect("topology etype is interned")
                .to_string();
            for dir in [Direction::Out, Direction::In] {
                for &nbr in self.topo.neighbors(etype, dir, id) {
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

    pub fn nodes_with_label(&self, label: &str) -> Vec<NodeRef<'_, F>> {
        self.view()
            .nodes_with_label(label)
            .into_iter()
            .map(|id| NodeRef { db: self, id })
            .collect()
    }

    pub fn find_nodes(&self, label: &str, filter: &Filter) -> Vec<NodeRef<'_, F>> {
        let view = self.view();
        view.nodes_with_label(label)
            .into_iter()
            .filter(|&id| eval_filter(filter, &|field| view.prop(id, field).cloned()))
            .map(|id| NodeRef { db: self, id })
            .collect()
    }

    /// Lex → parse → plan → execute `cypher` over a read-only view.
    /// Every pipeline `Err(String)` becomes `GraphError::QueryError` with a
    /// stage prefix (`lex:` / `parse:` / `plan:` / `execute:`).
    pub fn query(&self, cypher: &str, params: &BTreeMap<String, Value>) -> Result<ResultSet> {
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        let ast = parse(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        let ops = plan(&ast).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        execute(&self.view(), &ops, &Params(params)).map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })
    }

    /// Convenience entry-point that accepts a slice of `(name, value)` pairs
    /// instead of a pre-built `BTreeMap`.  Equivalent to building the map and
    /// calling [`GraphDb::query`].
    pub fn query_with_params(
        &self,
        cypher: &str,
        params: &[(&str, Value)],
    ) -> Result<ResultSet> {
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
    /// - SET RHS must be a literal; expression RHS → named error.
    /// - Combined read-write (`MATCH…SET…RETURN`) → named error.
    /// - `DETACH DELETE n` → calls `delete_node` for each matched node (removes all edges).
    /// - Bare `DELETE n` → error if n has any incident edges; succeeds for isolated nodes.
    /// - MERGE with ON CREATE/ON MATCH → named error.
    /// - Deleting a derived edge → named error "cannot delete derived edge".
    pub fn query_write(
        &mut self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResultSet> {
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
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
            WriteStatement::Create(s) => self.exec_create(s),
            WriteStatement::MatchSet(s) => self.exec_match_set(s, params),
            WriteStatement::MatchDelete(s) => self.exec_match_delete(s, params),
            WriteStatement::MatchDeleteNode(s) => self.exec_match_delete_node(s, params),
            WriteStatement::Merge(s) => self.exec_merge(s),
        }
    }

    fn exec_create(
        &mut self,
        stmt: core_query::cypher::CreateStmt,
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
            let src_key = var_to_key.get(&edge.src_var).ok_or_else(|| {
                GraphError::QueryError {
                    detail: format!(
                        "CREATE edge src variable '{}' is not bound",
                        edge.src_var
                    ),
                }
            })?;
            let dst_key = var_to_key.get(&edge.dst_var).ok_or_else(|| {
                GraphError::QueryError {
                    detail: format!(
                        "CREATE edge dst variable '{}' is not bound",
                        edge.dst_var
                    ),
                }
            })?;
            batch.insert_edge(&edge.etype, src_key, dst_key);
        }
        batch.commit()?;

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
        // Collect unique node vars targeted by SET clauses.
        let mut set_vars: Vec<String> = Vec::new();
        for s in &stmt.sets {
            if !set_vars.contains(&s.var) {
                set_vars.push(s.var.clone());
            }
        }

        // Synthesize a read query: MATCH … WHERE … RETURN <set_vars>
        let returns: Vec<RetItem> = set_vars
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
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        // MATCH phase is read-only; borrow ends before batch opens.
        let match_rs = execute(&self.view(), &ops, &Params(params))
            .map_err(|e| GraphError::QueryError {
                detail: format!("execute: {e}"),
            })?;

        // Collect (key, field, value) for each matched row × each SET clause.
        let mut set_ops: Vec<(String, String, Value)> = Vec::new();
        for row_i in 0..match_rs.len() {
            for sc in &stmt.sets {
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
                // Resolve the SET value operand — may be a literal or $param.
                use core_query::cypher::Operand;
                let value = match &sc.value {
                    Operand::Lit(v) => v.clone(),
                    Operand::Param(name) => {
                        params.get(name).cloned().ok_or_else(|| GraphError::QueryError {
                            detail: format!("missing parameter `{name}` in SET clause"),
                        })?
                    }
                    other => {
                        return Err(GraphError::QueryError {
                            detail: format!(
                                "SET value must be a literal or $param (got {other:?})"
                            ),
                        });
                    }
                };
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

        let mut rs = write_result_set();
        rs.push_row(vec![
            Some(Value::Int(0)),
            Some(Value::Int(props_set as i64)),
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
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        let match_rs = execute(&self.view(), &ops, &Params(params))
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
                detail:
                    "cannot delete derived edge; retract via the rule or change the property"
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
            order_by: vec![],
            skip: None,
            limit: None,
        };
        let ops = plan(&read_q).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        let match_rs = execute(&self.view(), &ops, &Params(params)).map_err(|e| {
            GraphError::QueryError {
                detail: format!("execute: {e}"),
            }
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
                    let has_edges = self.topo.etypes().any(|et| {
                        !self.topo.neighbors(et, Direction::Out, id).is_empty()
                            || !self.topo.neighbors(et, Direction::In, id).is_empty()
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
                    edges_deleted +=
                        (report.manual_edges + report.derived_edges) as i64;
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

    fn exec_merge(&mut self, stmt: core_query::cypher::MergeStmt) -> Result<ResultSet> {
        // MERGE: check if a node with the given key already exists.
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

        let mut created = 0i64;
        if !self.has_node(&key) {
            let props = vec![(stmt.key_field.clone(), stmt.key_value.clone())];
            self.batch()
                .insert_node(&stmt.label, &key, props)
                .commit()?;
            created = 1;
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
            let src_key = self
                .ids
                .key_of(src)
                .expect("provenance ids always resolvable")
                .to_string();
            let dst_key = self
                .ids
                .key_of(dst)
                .expect("provenance ids always resolvable")
                .to_string();
            let weight = rule_def.weight_prop.as_deref().and_then(|prop| {
                self.edge_props.get(etype, src, dst, prop).and_then(|v| {
                    if let Value::Float(f) = v {
                        Some(*f)
                    } else {
                        None
                    }
                })
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
            });
        }

        results.sort_by(|a, b| a.rule.cmp(&b.rule).then(a.edge_type.cmp(&b.edge_type)));
        Ok(results)
    }

    pub fn neighbors(&self, key: &str, edge_type: &str, dir: Direction) -> Result<Vec<String>> {
        let id = self
            .ids
            .get(key)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
        let Some(sym) = self.syms.get(edge_type) else {
            return Ok(Vec::new());
        };
        self.topo
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

    pub fn node_count(&self) -> usize {
        self.ids.len()
    }

    pub fn edge_count(&self) -> u64 {
        self.topo.edge_count()
    }

    /// Live/tombstone/edge counts plus per-rule provenance size, trip latch,
    /// and fire counter (includes rebuild evaluations). Rules are sorted by name.
    pub fn stats(&self) -> Stats {
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
            })
            .collect();
        Stats {
            nodes_live: self.ids.live_len(),
            nodes_tombstoned: self.ids.len() - self.ids.live_len(),
            edges: self.topo.edge_count(),
            rules,
        }
    }

    /// On-disk snapshot format version this binary writes and reads.
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

    /// Consume the db, returning its fs (for crash simulation).
    pub fn into_fs(self) -> F {
        self.fs
    }

    pub fn snapshot(&mut self) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        let (rule_defs_typed, provenance, rule_tripped, rule_fires) = self.engine.to_persist();
        let rule_defs = rule_defs_typed
            .iter()
            .map(|r| bincode::serialize(r).expect("RuleDef serialize cannot fail"))
            .collect();
        // Collect IVF state for approximate rules (V4).
        let raw_ivf = self.engine.export_ivf_state();
        let ivf_state: BTreeMap<String, core_storage::snapshot::PerRuleIvfState> = raw_ivf
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
        let state = core_storage::snapshot::SnapshotState {
            ids: self.ids.clone(),
            syms: self.syms.clone(),
            topo: self.topo.clone(),
            props: self.props.clone(),
            labels: self.labels.clone(),
            edge_props: self.edge_props.clone(),
            rule_defs,
            provenance,
            rule_tripped,
            rule_fires,
            ivf_state,
        };
        self.fs
            .write_atomic(FileId::Snapshot, &core_storage::snapshot::encode(&state))?;
        self.fs.write_atomic(FileId::Wal, b"")?; // wal tail now starts empty
        Ok(())
    }
}

/// Queued mutation for a [`BatchBuilder`].
enum BatchOp {
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
}

/// Overlay of ops already accepted earlier in the same batch. Never written
/// back to the database — validation only.
#[derive(Default)]
struct Overlay {
    extra_keys: BTreeSet<String>,
    deleted_keys: BTreeSet<String>,
    extra_props: BTreeMap<(String, String), Value>,
    removed_props: BTreeSet<(String, String)>,
    extra_edges: BTreeSet<(String, String, String)>,
    deleted_edges: BTreeSet<(String, String, String)>,
    extra_rules: BTreeSet<String>,
    deleted_rules: BTreeSet<String>,
}

/// Read-only view of live db state plus a batch overlay. Shared by single-op
/// public methods (empty overlay) and `commit_batch`.
struct MutPreview<'a, F: Fs> {
    db: &'a GraphDb<F>,
    overlay: Overlay,
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
        if self.overlay.extra_keys.contains(key) {
            return false;
        }
        self.db.get_prop(key, field).is_some()
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
        if self.overlay.extra_keys.contains(src_key) || self.overlay.extra_keys.contains(dst_key) {
            return false;
        }
        if self.overlay.deleted_keys.contains(src_key)
            || self.overlay.deleted_keys.contains(dst_key)
        {
            return false;
        }
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
            .topo
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
        if self.overlay.extra_keys.contains(src_key) || self.overlay.extra_keys.contains(dst_key) {
            return false;
        }
        if self.overlay.deleted_keys.contains(src_key)
            || self.overlay.deleted_keys.contains(dst_key)
        {
            return false;
        }
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

    fn check_insert_node(&self, key: &str) -> Result<()> {
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
        Ok(!self.has_edge(edge_type, src_key, dst_key))
    }

    fn prepare_remove_prop(&self, key: &str, field: &str) -> Result<bool> {
        self.check_live_key(key)?;
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
        // overlay; they cannot be provenance-owned yet either.
        let id = self.db.ids.get(key)?;
        let sym = self.db.labels.get(id as usize).copied()?;
        if sym == u32::MAX {
            return None;
        }
        self.db.syms.resolve(sym).map(str::to_string)
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
        if self.overlay.extra_keys.contains(key) {
            return None;
        }
        self.db.get_prop(key, field).cloned()
    }

    fn check_create_rule(&self, def: &RuleDef) -> Result<()> {
        def.validate()
            .map_err(|e| GraphError::RuleInvalid { detail: e })?;
        if self.has_rule(&def.name) {
            return Err(GraphError::RuleInvalid {
                detail: format!("rule {:?} already exists", def.name),
            });
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

    fn note_insert_node(&mut self, key: &str, props: &[(String, Value)]) {
        self.overlay.deleted_keys.remove(key);
        self.overlay.extra_keys.insert(key.to_string());
        self.overlay.extra_props.retain(|(k, _), _| k != key);
        self.overlay.removed_props.retain(|(k, _)| k != key);
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
        self.overlay.deleted_keys.insert(key.to_string());
        self.overlay.extra_props.retain(|(k, _), _| k != key);
        self.overlay.removed_props.retain(|(k, _)| k != key);
        self.overlay
            .extra_edges
            .retain(|(_, s, d)| s != key && d != key);
        self.overlay
            .deleted_edges
            .retain(|(_, s, d)| s != key && d != key);
    }

    fn note_create_rule(&mut self, name: &str) {
        self.overlay.deleted_rules.remove(name);
        self.overlay.extra_rules.insert(name.to_string());
    }

    fn note_delete_rule(&mut self, name: &str) {
        self.overlay.extra_rules.remove(name);
        self.overlay.deleted_rules.insert(name.to_string());
        // Treat the deleted rule's current provenance as gone so a later
        // delete_edge of those triples is a no-op (matches sequential).
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

    /// Same as [`commit`](Self::commit) but tail the inner events with
    /// [`MutationEvent::Ingested`] instead of [`MutationEvent::BatchApplied`].
    pub(crate) fn commit_ingest(&mut self, label: &str, inserted: usize) -> Result<(usize, usize)> {
        let ops = std::mem::take(&mut self.ops);
        self.db
            .commit_logged_batch(ops, Some((label.to_string(), inserted)))
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

    pub fn prop(&self, field: &str) -> Option<&Value> {
        self.db.props.get(self.id, field)
    }

    /// All stored fields for this node, sorted by field name.
    pub fn props(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        for field in self.db.props.fields() {
            if let Some(v) = self.db.props.get(self.id, field) {
                out.insert(field.to_string(), v.clone());
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
            let etype = view
                .syms
                .resolve(e.etype)
                .expect("topology etype is interned")
                .to_string();
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

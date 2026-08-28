//! Lock-free MVCC epoch readers.
//!
//! Each reader snapshots the db state at a fold point (every [`FOLD_EVERY_K`]
//! commits) plus a bounded delta tail, allowing concurrent reads without holding
//! the write lock.
//!
//! # Correctness guarantees
//! 1. **Snapshot isolation**: query returns results consistent with the db state
//!    at the moment [`GraphDb::reader`] was called.
//! 2. **RBAC mask coherence** (constraint 2): [`ReaderSnapshot::mask_for_role`]
//!    and [`ReaderSnapshot::query_masked`] operate on the same frozen base, so no
//!    node can slip through a stale mask.
//! 3. **Delta chain bounded**: at most `FOLD_EVERY_K − 1` deltas in the tail
//!    (fold resets the counter synchronously on the write path).

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, OnceLock};

use core_query::cypher::{execute, is_write_tokens, lex, parse, plan, Params};
use core_query::{neighborhood, Dir, GraphView, ResultSet};
use core_storage::fulltext::FulltextIndex;
use core_storage::v8::seam::{ColumnsView, TopologyView};
use core_storage::v8::MappedBase;
use core_storage::wal::WalRecord;
use core_storage::{
    ColumnStore, Direction, EdgeProps, EdgePropsView, GraphError, IdMap, Interner, Result,
    Topology, Value,
};

use crate::db::{EdgeInfo, NodeInfo};
use crate::mask::NodeMask;
use crate::roles::RoleDef;

/// Fold trigger: every K commits, the overlay is cloned into a new `FrozenOverlay`
/// and `delta_tail` is reset. The tail length is always ≤ K−1.
pub const FOLD_EVERY_K: usize = 64;

/// Per-commit overlay change record. Immutable after creation.
pub struct CommitDelta {
    /// WAL records for this commit (including `Intern` records, in WAL order).
    pub records: Vec<WalRecord>,
    /// Rule-derived edge **inserts** fired this commit: `(etype_sym, src_id, dst_id)`.
    pub derived_inserts: Vec<(u32, u32, u32)>,
    /// Rule-derived edge **retractions** this commit: `(etype_sym, src_id, dst_id)`.
    pub derived_deletes: Vec<(u32, u32, u32)>,
}

/// Full clone of overlay state captured at fold time.
///
/// The V8 mmap base is not cloned here; it is `Arc`-shared in `ReaderSnapshot`.
#[derive(Clone)]
pub struct FrozenOverlay {
    pub ids: IdMap,
    pub syms: Interner,
    pub topo: Topology,
    pub props: ColumnStore,
    pub labels: Vec<u32>,
    pub edge_props: EdgeProps,
    pub roles: Option<Vec<RoleDef>>,
    pub fulltext: FulltextIndex,
}

/// Lock-free reader snapshot: frozen overlay + optional V8 base + pending delta tail.
///
/// Obtained cheaply via [`crate::SharedDb::reader`], which acquires the read lock
/// only long enough to clone the `Arc` fields. Subsequent query operations run
/// without any lock.
pub struct ReaderSnapshot {
    /// Most-recent fold of the overlay state.
    pub frozen: Arc<FrozenOverlay>,
    /// Shared mmap base (zero-copy, Arc-ref-counted). `None` for legacy stores.
    pub base: Option<Arc<MappedBase>>,
    /// Commits since the last fold, in arrival order. Length ≤ `FOLD_EVERY_K − 1`.
    pub deltas: Vec<Arc<CommitDelta>>,
    /// Cached materialized overlay (frozen + deltas applied).
    ///
    /// Computed at most once per `ReaderSnapshot` on the first call to
    /// [`Self::effective`] when `deltas` is non-empty.  Stores `Err(String)` if
    /// WAL application fails so the error is returned to every subsequent caller
    /// without re-attempting.  When `deltas` is empty this field is never
    /// populated — `effective` returns `&frozen` directly.
    cache: OnceLock<std::result::Result<FrozenOverlay, String>>,
}

// ── Private view-building helpers ─────────────────────────────────────────────

fn build_tv<'a>(topo: &'a Topology, base: &'a Option<Arc<MappedBase>>) -> TopologyView<'a> {
    match base {
        None => TopologyView::owned(topo),
        Some(b) => {
            let csr = b.topology().expect("base CSR CRC already verified at open");
            TopologyView::with_base(topo, csr)
        }
    }
}

fn build_cv<'a>(props: &'a ColumnStore, base: &'a Option<Arc<MappedBase>>) -> ColumnsView<'a> {
    match base {
        None => ColumnsView::owned(props),
        Some(b) => {
            let cols = b
                .columns()
                .expect("base columns CRC already verified at open");
            ColumnsView::with_base(props, cols)
        }
    }
}

fn build_epv<'a>(
    edge_props: &'a EdgeProps,
    base: &'a Option<Arc<MappedBase>>,
) -> EdgePropsView<'a> {
    match base {
        None => EdgePropsView::owned(edge_props),
        Some(b) => {
            let archived = b
                .edge_props_section()
                .expect("base edge_props CRC already verified at open");
            EdgePropsView::with_base(edge_props, archived)
        }
    }
}

fn make_view<'a>(
    state: &'a FrozenOverlay,
    base: &'a Option<Arc<MappedBase>>,
    mask: Option<&'a HashSet<u32>>,
) -> GraphView<'a> {
    GraphView {
        ids: &state.ids,
        syms: &state.syms,
        labels: &state.labels,
        props: build_cv(&state.props, base),
        topo: build_tv(&state.topo, base),
        edge_props: build_epv(&state.edge_props, base),
        mask,
    }
}

// ── Delta application ─────────────────────────────────────────────────────────

/// Apply a single WAL record (recursing into `Batch`) to mutable working state.
/// Skips rule/view records that are no-ops on the read path.
///
/// Note: fulltext is updated incrementally here for correctness; after all deltas
/// are applied the caller should call `fulltext.rebuild_all` to correct drift from
/// multi-field updates and out-of-order incremental additions.
#[allow(clippy::too_many_arguments)]
fn apply_one(
    ids: &mut IdMap,
    syms: &mut Interner,
    topo: &mut Topology,
    props: &mut ColumnStore,
    edge_props: &mut EdgeProps,
    labels: &mut Vec<u32>,
    fulltext: &mut FulltextIndex,
    rec: &WalRecord,
) -> Result<()> {
    match rec {
        WalRecord::Intern { id, text } => {
            let got = syms.intern(text);
            if got != *id {
                return Err(GraphError::Corrupt {
                    detail: format!(
                        "mvcc delta intern mismatch for {text:?}: expected {id}, got {got}"
                    ),
                });
            }
        }

        WalRecord::InsertNodeId {
            label,
            key,
            props: node_props,
        } => {
            let node_id = ids.try_insert(key)?;
            if labels.len() <= node_id as usize {
                labels.resize(node_id as usize + 1, u32::MAX);
            }
            labels[node_id as usize] = *label;
            let label_str = syms
                .resolve(*label)
                .ok_or_else(|| GraphError::Corrupt {
                    detail: format!("mvcc delta: unknown label sym {label}"),
                })?
                .to_string();
            for (field_sym, value) in node_props {
                let field = syms
                    .resolve(*field_sym)
                    .ok_or_else(|| GraphError::Corrupt {
                        detail: format!("mvcc delta: unknown field sym {field_sym}"),
                    })?
                    .to_string();
                props.set(node_id, &field, value.clone());
                if fulltext.is_enabled(&label_str, &field) {
                    fulltext.add_tokens(node_id, &field, value);
                }
            }
        }

        WalRecord::InsertNode {
            label,
            key,
            props: node_props,
        } => {
            let label_sym = syms.intern(label);
            let node_id = ids.try_insert(key)?;
            if labels.len() <= node_id as usize {
                labels.resize(node_id as usize + 1, u32::MAX);
            }
            labels[node_id as usize] = label_sym;
            for (field, value) in node_props {
                props.set(node_id, field, value.clone());
                if fulltext.is_enabled(label, field) {
                    fulltext.add_tokens(node_id, field, value);
                }
            }
        }

        WalRecord::SetPropId { id, field, value } => {
            if let Some(field_str) = syms.resolve(*field).map(str::to_string) {
                props.set(*id, &field_str, value.clone());
                if let Some(&label_sym) = labels.get(*id as usize) {
                    if let Some(label_str) = syms.resolve(label_sym) {
                        if fulltext.is_enabled(label_str, &field_str) {
                            fulltext.add_tokens(*id, &field_str, value);
                        }
                    }
                }
            }
        }

        WalRecord::SetProp { key, field, value } => {
            if let Some(node_id) = ids.get(key) {
                props.set(node_id, field, value.clone());
                if let Some(&label_sym) = labels.get(node_id as usize) {
                    if let Some(label_str) = syms.resolve(label_sym) {
                        if fulltext.is_enabled(label_str, field) {
                            fulltext.add_tokens(node_id, field, value);
                        }
                    }
                }
            }
        }

        WalRecord::RemoveProp { key, field } => {
            if let Some(node_id) = ids.get(key) {
                props.remove(node_id, field);
                fulltext.remove_node_field(node_id, field);
            }
        }

        WalRecord::DeleteNode { key } => {
            if let Some(node_id) = ids.delete(key) {
                props.remove_all(node_id);
                fulltext.remove_node(node_id);
                // Mark the label slot as sentinel so label_of returns None.
                if let Some(slot) = labels.get_mut(node_id as usize) {
                    *slot = u32::MAX;
                }
                // Sweep all edges incident on this node to prevent phantom
                // adjacency. db.rs deletes these edges inline without emitting
                // DeleteEdge WAL records, so we must mirror that sweep here.
                let etypes: Vec<u32> = topo.etypes().collect();
                let mut doomed = Vec::new();
                for et in &etypes {
                    for &dst in topo.neighbors(*et, Direction::Out, node_id).as_ref() {
                        doomed.push((*et, node_id, dst));
                    }
                    for &src in topo.neighbors(*et, Direction::In, node_id).as_ref() {
                        doomed.push((*et, src, node_id));
                    }
                }
                for (et, s, d) in doomed {
                    topo.remove_edge(et, s, d);
                    edge_props.remove_edge(et, s, d);
                }
            }
        }

        WalRecord::InsertEdgeId { etype, src, dst } => {
            topo.add_edge(*etype, *src, *dst);
        }

        WalRecord::InsertEdge {
            edge_type,
            src_key,
            dst_key,
        } => {
            let etype = syms.intern(edge_type);
            if let (Some(src), Some(dst)) = (ids.get(src_key), ids.get(dst_key)) {
                topo.add_edge(etype, src, dst);
            }
        }

        WalRecord::DeleteEdge {
            edge_type,
            src_key,
            dst_key,
        } => {
            if let Some(etype) = syms.get(edge_type) {
                if let (Some(src), Some(dst)) = (ids.get(src_key), ids.get(dst_key)) {
                    topo.remove_edge(etype, src, dst);
                }
            }
        }

        WalRecord::EnableFulltext { label, field } => {
            fulltext.enable(label, field);
        }

        WalRecord::DisableFulltext { label, field } => {
            fulltext.disable(label, field);
        }

        WalRecord::Batch(inner) => {
            for r in inner {
                apply_one(ids, syms, topo, props, edge_props, labels, fulltext, r)?;
            }
        }

        // No-ops for the read path: rule and view management do not affect
        // the structural overlay data that queries read.
        WalRecord::CreateRule { .. }
        | WalRecord::DeleteRule { .. }
        | WalRecord::RebuildRule { .. }
        | WalRecord::CreateView { .. }
        | WalRecord::DeleteView { .. } => {}
    }
    Ok(())
}

// ── ReaderSnapshot ────────────────────────────────────────────────────────────

fn mask_for_role_from(state: &FrozenOverlay, role: &str) -> Result<NodeMask> {
    let roles = state.roles.as_ref().ok_or_else(|| GraphError::Corrupt {
        detail: "roles.json was corrupt at open; fix the file and re-open".into(),
    })?;
    let def = roles
        .iter()
        .find(|r| r.name == role)
        .ok_or_else(|| GraphError::KeyNotFound {
            key: format!("role:{role}"),
        })?;
    let mut visible = HashSet::new();
    for key in &def.keys {
        if let Some(id) = state.ids.get(key) {
            visible.insert(id);
        }
    }
    for label_name in &def.labels {
        if let Some(sym) = state.syms.get(label_name) {
            for (i, &s) in state.labels.iter().enumerate() {
                if s == sym {
                    visible.insert(i as u32);
                }
            }
        }
    }
    Ok(NodeMask { visible })
}

impl ReaderSnapshot {
    /// Apply all pending deltas to a clone of `frozen`.
    ///
    /// Returns the frozen state (cloned) with delta changes applied, including
    /// rule-derived edge inserts/retracts and a rebuilt full-text index.
    fn materialize(&self) -> Result<FrozenOverlay> {
        let mut w = (*self.frozen).clone();
        for delta in &self.deltas {
            for rec in &delta.records {
                apply_one(
                    &mut w.ids,
                    &mut w.syms,
                    &mut w.topo,
                    &mut w.props,
                    &mut w.edge_props,
                    &mut w.labels,
                    &mut w.fulltext,
                    rec,
                )?;
            }
            for &(etype, src, dst) in &delta.derived_inserts {
                w.topo.add_edge(etype, src, dst);
            }
            for &(etype, src, dst) in &delta.derived_deletes {
                w.topo.remove_edge(etype, src, dst);
            }
        }
        if !self.deltas.is_empty() {
            // Rebuild full-text to correct incremental drift accumulated during
            // delta application (add_tokens is imprecise for multi-field/deletion paths).
            let cv = build_cv(&w.props, &self.base);
            w.fulltext.rebuild_all(&w.ids, &w.labels, &w.syms, cv);
        }
        Ok(w)
    }

    /// Construct a `ReaderSnapshot` from its constituent parts.
    ///
    /// Used by [`crate::db::GraphDb::reader`] — the only site that builds a
    /// snapshot — so the private `cache` field stays encapsulated here.
    pub(crate) fn new(
        frozen: Arc<FrozenOverlay>,
        base: Option<Arc<MappedBase>>,
        deltas: Vec<Arc<CommitDelta>>,
    ) -> Self {
        Self {
            frozen,
            base,
            deltas,
            cache: OnceLock::new(),
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Return a reference to the current effective state.
    ///
    /// When the delta tail is empty this is a zero-copy borrow of `frozen`.
    /// Otherwise the delta tail is applied to a clone of `frozen` exactly once
    /// (cached in `self.cache`) so that all operations within a single
    /// `ReaderSnapshot` share the same materialized view (F3: no triple
    /// materialize per request).
    fn effective(&self) -> Result<&FrozenOverlay> {
        if self.deltas.is_empty() {
            return Ok(&self.frozen);
        }
        let cached = self
            .cache
            .get_or_init(|| self.materialize().map_err(|e| e.to_string()));
        cached
            .as_ref()
            .map_err(|e| GraphError::Corrupt { detail: e.clone() })
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Resolve a role name to a node visibility mask.
    ///
    /// Coherent with [`Self::query_masked`]: both read from the same effective
    /// state (frozen or cached materialization), so the mask is never stale
    /// relative to the query data.
    pub fn mask_for_role(&self, role: &str) -> Result<NodeMask> {
        mask_for_role_from(self.effective()?, role)
    }

    /// Resolve a node key to its dense id.
    ///
    /// Checks the delta tail (via the cached materialization) so that nodes
    /// inserted since the last fold are visible.
    pub fn resolve_key(&self, key: &str) -> Option<u32> {
        self.effective().ok()?.ids.get(key)
    }

    /// Execute a read-only Cypher query over the epoch snapshot.
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
        let state = self.effective()?;
        let view = make_view(state, &self.base, None);
        execute(&view, &ops, &Params(params)).map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })
    }

    /// Execute a read-only Cypher query with a node visibility mask.
    ///
    /// Returns `Err` when `cypher` is a write statement (CREATE / MATCH…SET / DELETE).
    pub fn query_masked(
        &self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
        mask: &NodeMask,
    ) -> Result<ResultSet> {
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        if is_write_tokens(&tokens) {
            return Err(GraphError::QueryError {
                detail: "masked queries are read-only".into(),
            });
        }
        let ast = parse(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        let ops = plan(&ast).map_err(|e| GraphError::QueryError {
            detail: format!("plan: {e}"),
        })?;
        let state = self.effective()?;
        let view = make_view(state, &self.base, Some(&mask.visible));
        execute(&view, &ops, &Params(params)).map_err(|e| GraphError::QueryError {
            detail: format!("execute: {e}"),
        })
    }

    /// Live node info from the epoch snapshot. `None` if key is absent or tombstoned.
    pub fn node_info(&self, key: &str) -> Option<NodeInfo> {
        node_info_from(key, self.effective().ok()?, &self.base)
    }

    /// Every directed edge incident on `key`. `derived` is always `false` since
    /// the reader snapshot has no rule engine.
    ///
    /// Unknown key → `Err(GraphError::KeyNotFound)`.
    pub fn node_edges(&self, key: &str) -> Result<Vec<EdgeInfo>> {
        node_edges_from(key, self.effective()?, &self.base)
    }

    /// BFS neighborhood expansion restricted to `mask`-visible nodes.
    ///
    /// Hidden nodes are neither returned nor used as traversal intermediaries
    /// (never-leak invariant). Returns `None` when `key` does not exist.
    pub fn neighborhood_masked(
        &self,
        key: &str,
        depth: u32,
        edge_types: Option<&[&str]>,
        dir: Dir,
        mask: &NodeMask,
    ) -> Option<ResultSet> {
        neighborhood_masked_from(
            key,
            self.effective().ok()?,
            &self.base,
            depth,
            edge_types,
            dir,
            mask,
        )
    }
}

// ── Free-standing helpers that take state by reference ────────────────────────

fn node_info_from(
    key: &str,
    state: &FrozenOverlay,
    base: &Option<Arc<MappedBase>>,
) -> Option<NodeInfo> {
    let id = state.ids.get(key)?;
    let label_sym = *state.labels.get(id as usize)?;
    if label_sym == u32::MAX {
        return None;
    }
    let label = state.syms.resolve(label_sym)?.to_string();
    let cv = build_cv(&state.props, base);
    let mut props = BTreeMap::new();
    for field in cv.field_names() {
        if let Some(vr) = cv.get(id, &field) {
            props.insert(field, vr.into_value());
        }
    }
    Some(NodeInfo {
        key: key.to_string(),
        label,
        props,
    })
}

fn node_edges_from(
    key: &str,
    state: &FrozenOverlay,
    base: &Option<Arc<MappedBase>>,
) -> Result<Vec<EdgeInfo>> {
    let id = state
        .ids
        .get(key)
        .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
    let tv = build_tv(&state.topo, base);
    let mut edges = Vec::new();
    for etype in tv.etypes() {
        let edge_type = state
            .syms
            .resolve(etype)
            .expect("topology etype must be interned")
            .to_string();
        for dir in [Direction::Out, Direction::In] {
            for &nbr in tv.neighbors(etype, dir, id).as_ref() {
                let (src_key, dst_key) = match dir {
                    Direction::Out => (
                        key.to_string(),
                        state
                            .ids
                            .key_of(nbr)
                            .ok_or_else(|| GraphError::Corrupt {
                                detail: format!("topology id {nbr} has no key"),
                            })?
                            .to_string(),
                    ),
                    Direction::In => (
                        state
                            .ids
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
                    derived: false,
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
    edges.dedup();
    Ok(edges)
}

fn neighborhood_masked_from(
    key: &str,
    state: &FrozenOverlay,
    base: &Option<Arc<MappedBase>>,
    depth: u32,
    edge_types: Option<&[&str]>,
    dir: Dir,
    mask: &NodeMask,
) -> Option<ResultSet> {
    let id = state.ids.get(key)?;
    let view = make_view(state, base, Some(&mask.visible));
    let resolved: Option<Vec<u32>> = edge_types.map(|names| {
        names
            .iter()
            .filter_map(|name| view.syms.get(name))
            .collect()
    });
    let nb = neighborhood(&view, id, depth, resolved.as_deref(), dir);
    let mut rs = ResultSet::new(vec!["key".into(), "label".into(), "depth".into()]);
    for (nid, d) in nb.nodes {
        let k = view.key_of(nid);
        let lbl = view
            .label_of(nid)
            .expect("real nodes always have a label; u32::MAX sentinel cannot occur");
        rs.push_row(vec![
            Some(Value::Str(k.to_string())),
            Some(Value::Str(lbl.to_string())),
            Some(Value::Int(d as i64)),
        ]);
    }
    Some(rs)
}

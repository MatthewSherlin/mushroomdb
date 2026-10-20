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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use core_query::cypher::{execute, is_write_tokens, lex, parse, plan, Params};
use core_query::{expand, neighborhood, Dir, GraphView, ResultSet};
use core_storage::fulltext::FulltextIndex;
use core_storage::v8::seam::{ColumnsView, TopologyView};
use core_storage::v8::MappedBase;
use core_storage::wal::WalRecord;
use core_storage::{
    ColumnStore, Direction, EdgeProps, EdgePropsView, GraphError, IdMap, Interner, Result,
    Topology, Value,
};

use crate::db::{EdgeInfo, NodeInfo};
use crate::mask::{NodeMask, RoleMaskCache};
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
    /// The same changes, indexed by node, for `node_edges`. Each triple appears
    /// once, in the state this commit left it: the two lists above never name
    /// the same triple.
    pub derived: DerivedChanges,
    /// View properties as this commit left them: `(node_id, view_prop, value)`,
    /// `None` where the view has no value for the node. A view's value is in
    /// no record — the writer computes it from the edges and properties the
    /// records change — so the reader is handed the result rather than made
    /// to recompute it.
    pub view_values: Vec<(u32, String, Option<Value>)>,
}

/// An edge as the rule engine owns it: `(etype_sym, src_id, dst_id)`.
pub type DerivedTriple = (u32, u32, u32);

/// Layers a fold carries before it compacts them into one.
const MAX_DERIVED_LAYERS: usize = 8;

/// Node lists longer than this are kept sorted and searched; shorter ones are
/// scanned, which is faster and needs no sort at build.
const SCAN_LIMIT: usize = 8;

/// The derived triples touching each node, as two flat arrays (compressed rows):
/// `entries[offsets[n]..offsets[n + 1]]` is every triple with `n` as its source
/// or its destination, a triple listed under both. Built by counting, with no
/// per-node allocation and no hashing; a `node_edges` call reads one row.
struct NodeIndex {
    offsets: Vec<u32>,
    entries: Vec<DerivedTriple>,
}

impl NodeIndex {
    fn empty() -> Self {
        Self {
            offsets: Vec::new(),
            entries: Vec::new(),
        }
    }

    fn build(triples: &[DerivedTriple]) -> Self {
        let Some(max_node) = triples.iter().map(|&(_, src, dst)| src.max(dst)).max() else {
            return Self::empty();
        };
        let rows = max_node as usize + 1;
        let mut offsets = vec![0u32; rows + 1];
        for &(_, src, dst) in triples {
            offsets[src as usize + 1] += 1;
            if dst != src {
                offsets[dst as usize + 1] += 1;
            }
        }
        for row in 0..rows {
            offsets[row + 1] += offsets[row];
        }
        let mut next = offsets.clone();
        let mut entries = vec![(0, 0, 0); offsets[rows] as usize];
        for &triple @ (_, src, dst) in triples {
            entries[next[src as usize] as usize] = triple;
            next[src as usize] += 1;
            if dst != src {
                entries[next[dst as usize] as usize] = triple;
                next[dst as usize] += 1;
            }
        }
        for row in 0..rows {
            let (from, to) = (offsets[row] as usize, offsets[row + 1] as usize);
            if to - from > SCAN_LIMIT {
                entries[from..to].sort_unstable();
            }
        }
        Self { offsets, entries }
    }

    fn row(&self, node: u32) -> &[DerivedTriple] {
        let node = node as usize;
        match (self.offsets.get(node), self.offsets.get(node + 1)) {
            (Some(&from), Some(&to)) => &self.entries[from as usize..to as usize],
            _ => &[],
        }
    }
}

fn row_contains(row: &[DerivedTriple], triple: DerivedTriple) -> bool {
    if row.len() <= SCAN_LIMIT {
        row.contains(&triple)
    } else {
        row.binary_search(&triple).is_ok()
    }
}

/// A set of changes to which edges are derived — `true` where a rule owns the
/// triple afterwards, `false` where none does — grouped by node, so that a
/// `node_edges` call finds a node's changes with one search however many edges
/// the node has. The nodes are a sorted array, searched in place: for the few
/// nodes a commit touches that beats hashing, and a read probes up to 63 of
/// these.
///
/// One per commit (in its [`CommitDelta`]) and one per fold layer.
#[derive(Default)]
pub struct DerivedChanges {
    /// Nodes with changes, ascending.
    nodes: Vec<u32>,
    /// `entries[starts[i]..starts[i + 1]]` are the changes touching `nodes[i]`,
    /// ascending by triple; a triple is listed under both of its endpoints.
    starts: Vec<u32>,
    entries: Vec<(DerivedTriple, bool)>,
    /// Distinct triples.
    count: usize,
}

impl DerivedChanges {
    /// Index a net set of changes: one final state per triple.
    pub fn from_net(net: &HashMap<DerivedTriple, bool>) -> Self {
        let mut pairs: Vec<(u32, DerivedTriple, bool)> = Vec::with_capacity(net.len() * 2);
        for (&triple, &state) in net {
            pairs.push((triple.1, triple, state));
            if triple.2 != triple.1 {
                pairs.push((triple.2, triple, state));
            }
        }
        pairs.sort_unstable_by_key(|&(node, triple, _)| (node, triple));
        let mut nodes = Vec::new();
        let mut starts = Vec::new();
        let mut entries = Vec::with_capacity(pairs.len());
        for (node, triple, state) in pairs {
            if nodes.last() != Some(&node) {
                nodes.push(node);
                starts.push(entries.len() as u32);
            }
            entries.push((triple, state));
        }
        starts.push(entries.len() as u32);
        Self {
            nodes,
            starts,
            entries,
            count: net.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The changes touching `node`, sorted by triple.
    fn for_node(&self, node: u32) -> &[(DerivedTriple, bool)] {
        match self.nodes.binary_search(&node) {
            Ok(i) => &self.entries[self.starts[i] as usize..self.starts[i + 1] as usize],
            Err(_) => &[],
        }
    }

    fn net(&self) -> HashMap<DerivedTriple, bool> {
        let mut net = HashMap::with_capacity(self.count);
        for (i, &node) in self.nodes.iter().enumerate() {
            let row = &self.entries[self.starts[i] as usize..self.starts[i + 1] as usize];
            net.extend(row.iter().filter(|(t, _)| t.1 == node).copied());
        }
        net
    }
}

enum DerivedRoot {
    /// Taken from the engine's provenance when the fold was built.
    Index(NodeIndex),
    /// The provenance section of the snapshot base, indexed by the first read
    /// that needs it.
    Base(OnceLock<NodeIndex>),
}

/// A [`DerivedRoot`] resolved against the base it reads.
struct RootRef<'a>(Option<&'a NodeIndex>);

impl<'a> RootRef<'a> {
    fn row(&self, node: u32) -> &'a [DerivedTriple] {
        self.0.map_or(&[], |index| index.row(node))
    }

    fn contains(&self, triple: DerivedTriple) -> bool {
        row_contains(self.row(triple.1), triple)
    }
}

/// The rule-derived edges as of a fold: what `node_edges` consults to say
/// whether an edge is derived. The reader holds no rule engine, so the writer
/// hands it the answer.
///
/// A root, taken when the state is loaded, under a few layers of changes, one
/// per fold since. A fold on the write path costs what its commits changed, not
/// the size of the store's provenance, and when the root is the snapshot base's
/// PROVENANCE section it is read where it lies.
#[derive(Clone)]
pub struct DerivedEdges {
    root: Arc<DerivedRoot>,
    /// Oldest first; a later layer overrides an earlier one and the root.
    layers: Vec<Arc<DerivedChanges>>,
}

impl DerivedEdges {
    /// A root taken from the engine's live provenance.
    pub fn from_triples(triples: impl IntoIterator<Item = DerivedTriple>) -> Self {
        let triples: Vec<DerivedTriple> = triples.into_iter().collect();
        Self {
            root: Arc::new(DerivedRoot::Index(NodeIndex::build(&triples))),
            layers: Vec::new(),
        }
    }

    /// A root that is the provenance section of the base the fold is taken over.
    pub fn from_base() -> Self {
        Self {
            root: Arc::new(DerivedRoot::Base(OnceLock::new())),
            layers: Vec::new(),
        }
    }

    fn root_ref<'a>(&'a self, base: &'a Option<Arc<MappedBase>>) -> RootRef<'a> {
        match self.root.as_ref() {
            DerivedRoot::Index(index) => RootRef(Some(index)),
            DerivedRoot::Base(cell) => {
                let Some(base) = base else {
                    return RootRef(None);
                };
                RootRef(Some(cell.get_or_init(|| {
                    let bytes = base.provenance_raw_bytes().unwrap_or_default();
                    core_storage::v8::encode::provenance_triples(bytes)
                        .map_or_else(NodeIndex::empty, |triples| NodeIndex::build(&triples))
                })))
            }
        }
    }

    /// The next fold's derived edges: these, with `changes` (each triple's final
    /// state since the last fold) laid over them.
    ///
    /// Past [`MAX_DERIVED_LAYERS`] the layers are merged, and what the merge
    /// leaves that the root already says is dropped, so the layers hold the
    /// difference from the root and no more.
    pub fn extended(
        &self,
        changes: HashMap<DerivedTriple, bool>,
        base: &Option<Arc<MappedBase>>,
    ) -> Self {
        let mut next = self.clone();
        if changes.is_empty() {
            return next;
        }
        if next.layers.len() < MAX_DERIVED_LAYERS {
            next.layers
                .push(Arc::new(DerivedChanges::from_net(&changes)));
            return next;
        }
        let mut merged = HashMap::new();
        for layer in &next.layers {
            merged.extend(layer.net());
        }
        merged.extend(changes);
        let root = self.root_ref(base);
        merged.retain(|&triple, &mut state| state != root.contains(triple));
        next.layers = if merged.is_empty() {
            Vec::new()
        } else {
            vec![Arc::new(DerivedChanges::from_net(&merged))]
        };
        next
    }
}

/// Full clone of overlay state captured at fold time.
///
/// The V8 mmap base is not cloned here; it is `Arc`-shared in `ReaderSnapshot`.
#[derive(Clone)]
pub struct FrozenOverlay {
    pub ids: Arc<IdMap>,
    pub syms: Arc<Interner>,
    pub topo: Arc<Topology>,
    pub props: Arc<ColumnStore>,
    pub labels: Arc<Vec<u32>>,
    pub edge_props: Arc<EdgeProps>,
    pub roles: Option<Arc<Vec<RoleDef>>>,
    pub fulltext: Arc<FulltextIndex>,
    /// Which edges in `topo` (and the base under it) a rule derived.
    pub derived: DerivedEdges,
}

/// Lock-free reader snapshot: frozen overlay + optional V8 base + pending delta tail.
///
/// Obtained cheaply via [`crate::SharedDb::reader`], which acquires the read lock
/// only long enough to clone the `Arc` fields. Subsequent query operations run
/// without any lock.
///
/// **Memory bound**: holds at most `FOLD_EVERY_K − 1` [`CommitDelta`] `Arc`s in
/// its delta tail; the fold that resets the tail runs synchronously on the write path.
pub struct ReaderSnapshot {
    /// Most-recent fold of the overlay state.
    pub frozen: Arc<FrozenOverlay>,
    /// Shared mmap base (zero-copy, Arc-ref-counted). `None` for legacy stores.
    pub base: Option<Arc<MappedBase>>,
    /// Commits since the last fold, in arrival order. Length ≤ `FOLD_EVERY_K − 1`.
    pub deltas: Vec<Arc<CommitDelta>>,
    /// The store's `commit_seq` when this snapshot was taken — the version key
    /// for the shared role-mask memo. The effective state (frozen + deltas) is
    /// exactly the state the live handle had at this commit.
    pub version: u64,
    /// Role → mask memo, shared with the `GraphDb` this snapshot came from.
    role_masks: Arc<RoleMaskCache>,
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
            let strings = b
                .string_table()
                .transpose()
                .expect("base strings CRC already verified at open");
            ColumnsView::with_base_cached(props, cols, b.mixed_cache()).with_shared_strings(strings)
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
    mask: Option<&'a core_query::visible::VisibleSet>,
) -> GraphView<'a> {
    GraphView {
        ids: &state.ids,
        syms: &state.syms,
        labels: &state.labels,
        props: build_cv(&state.props, base),
        topo: build_tv(&state.topo, base),
        edge_props: build_epv(&state.edge_props, base),
        mask,
        // MVCC reader snapshots don't carry the equality index; IndexScan
        // falls back to a correct scan+filter on this path.
        prop_index: None,
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
    base: &Option<Arc<MappedBase>>,
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
                // A base-resident prop must be masked or ColumnsView falls
                // through to the archived value (mirrors db.rs WAL replay).
                // Tombstoning a prop absent from the base is a harmless
                // false-mask: the overlay short-circuit never reaches it.
                props.record_prop_tombstone(node_id, field);
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
                // DeleteEdge WAL records, so we must mirror that sweep here —
                // over the snapshot's edges as well as the overlay's, as
                // db.rs does.
                let mut doomed = Vec::new();
                {
                    let tv = match base {
                        None => TopologyView::owned(topo),
                        Some(b) => match b.topology() {
                            Ok(archived) => TopologyView::with_base(topo, archived),
                            Err(_) => TopologyView::owned(topo),
                        },
                    };
                    let etypes: Vec<u32> = tv.etypes().collect();
                    for et in &etypes {
                        for &dst in tv.neighbors(*et, Direction::Out, node_id).as_ref() {
                            doomed.push((*et, node_id, dst));
                        }
                        for &src in tv.neighbors(*et, Direction::In, node_id).as_ref() {
                            if src != node_id {
                                doomed.push((*et, src, node_id));
                            }
                        }
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
                apply_one(ids, syms, topo, props, edge_props, labels, fulltext, base, r)?;
            }
        }

        // No-ops for the read path: rule and view management do not affect
        // the structural overlay data that queries read.
        WalRecord::CreateRule { .. }
        | WalRecord::DeleteRule { .. }
        | WalRecord::RebuildRule { .. }
        | WalRecord::CreateView { .. }
        | WalRecord::DeleteView { .. }
        // Property-index declarations are no-ops in the read path: MVCC reader
        // snapshots do not carry the equality index, so `IndexScan` falls back
        // to a correct scan+filter for reader queries.
        | WalRecord::EnableIndex { .. }
        | WalRecord::DisableIndex { .. }
        // The opt-in declaration is a writer-side gate; the read path never
        // decides whether to write a record. A *count* is not a no-op and is
        // handled below.
        | WalRecord::SetEdgeCount { count: 0, .. }
        // History markers are no-ops in the read path. Rules re-derive their
        // edges when the ReaderSnapshot queries the live engine; markers only
        // serve edge_history / was_linked WAL scans.
        | WalRecord::DerivedEdgeAdded { .. }
        | WalRecord::DerivedEdgeRetracted { .. } => {}

        // An insert count is an edge property, so the read path carries it the
        // way it carries any other: absolute, last write wins.
        WalRecord::SetEdgeCount {
            etype,
            src,
            dst,
            count,
        } => {
            edge_props.set(
                *etype,
                *src,
                *dst,
                crate::db::EDGE_COUNT_PROP,
                core_storage::Value::Int(*count as i64),
            );
        }

        WalRecord::RenameNode { old_key, new_key } => {
            // Recovery-safe: if old_key is already gone (frozen overlay or a
            // prior delta already applied the rename), skip cleanly.
            if ids.get(old_key).is_some() {
                ids.rename(old_key, new_key).map_err(|_| GraphError::Corrupt {
                    detail: format!("mvcc delta RenameNode {old_key}→{new_key} failed"),
                })?;
            }
        }
    }
    Ok(())
}

// ── ReaderSnapshot ────────────────────────────────────────────────────────────

/// Resolve `role` against a frozen overlay — the reader-side twin of
/// [`crate::db::GraphDb::mask_for_role`], and kept identical to it.
///
/// Takes `base` because a role carrying a `visible_where` predicate has to read
/// properties, and a node's property may live in the mmap'd base rather than
/// the overlay.
fn mask_for_role_from(
    state: &FrozenOverlay,
    base: &Option<Arc<MappedBase>>,
    role: &str,
) -> Result<NodeMask> {
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
    // Key leg: an administrative grant, never narrowed by the predicate.
    for key in &def.keys {
        if let Some(id) = state.ids.get(key) {
            visible.insert(id);
        }
    }
    let props = def
        .visible_where
        .as_ref()
        .map(|_| build_cv(&state.props, base));
    for label_name in &def.labels {
        if let Some(sym) = state.syms.get(label_name) {
            for (i, &s) in state.labels.iter().enumerate() {
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
    // Namespace leg — the live resolver's retain, against the frozen overlay's
    // own `ns` column (the reader has no derived `node_ns` array: its effective
    // state is assembled per snapshot, and reading the column it would mirror is
    // the same answer by construction). Intersects the key leg too; see
    // `RoleDef::namespaces`.
    if def.namespaces.is_some() {
        let cv = build_cv(&state.props, base);
        visible.retain(|&id| {
            let value = cv.get(id, core_storage::NS_PROP).map(|vr| vr.into_value());
            def.sees_namespace(core_storage::namespace_of_value(value.as_ref()))
        });
    }
    Ok(NodeMask::from_ids(visible))
}

impl ReaderSnapshot {
    /// Apply all pending deltas to a clone of `frozen`.
    ///
    /// Returns the frozen state (cloned) with delta changes applied, including
    /// rule-derived edge inserts/retracts and a rebuilt full-text index.
    fn materialize(&self) -> Result<FrozenOverlay> {
        // Cloning the struct now clones eight `Arc`s — refcount bumps, not data.
        // Each `Arc::make_mut` below copies exactly one structure, once, and only
        // because this snapshot is about to mutate its own working copy. The
        // fold this came from keeps its allocation, which is what preserves the
        // isolation `tests/reader_isolation.rs` pins.
        //
        // Before the fields were `Arc`, this deep-copied all eight every time a
        // snapshot with a non-empty delta tail was read. Now it copies only what
        // the deltas actually touch.
        let mut w = (*self.frozen).clone();
        for delta in &self.deltas {
            for rec in &delta.records {
                apply_one(
                    Arc::make_mut(&mut w.ids),
                    Arc::make_mut(&mut w.syms),
                    Arc::make_mut(&mut w.topo),
                    Arc::make_mut(&mut w.props),
                    Arc::make_mut(&mut w.edge_props),
                    Arc::make_mut(&mut w.labels),
                    Arc::make_mut(&mut w.fulltext),
                    &self.base,
                    rec,
                )?;
            }
            for &(etype, src, dst) in &delta.derived_inserts {
                Arc::make_mut(&mut w.topo).add_edge(etype, src, dst);
            }
            for &(etype, src, dst) in &delta.derived_deletes {
                Arc::make_mut(&mut w.topo).remove_edge(etype, src, dst);
            }
            for (node, view_prop, value) in &delta.view_values {
                let props = Arc::make_mut(&mut w.props);
                match value {
                    Some(v) => props.set(*node, view_prop, v.clone()),
                    None => {
                        // Tombstoned as well as removed, or the snapshot's
                        // value is read through (mirrors `clear_view_value`).
                        props.remove(*node, view_prop);
                        props.record_prop_tombstone(*node, view_prop);
                    }
                }
            }
        }
        if !self.deltas.is_empty() {
            // Rebuild full-text to correct incremental drift accumulated during
            // delta application (add_tokens is imprecise for multi-field/deletion paths).
            let cv = build_cv(&w.props, &self.base);
            let (ids, labels, syms) = (w.ids.clone(), w.labels.clone(), w.syms.clone());
            Arc::make_mut(&mut w.fulltext).rebuild_all(&ids, &labels, &syms, cv);
        }
        Ok(w)
    }

    /// What the delta tail, the fold's layers and its root say about which of
    /// `node`'s edges are derived.
    fn derived_lookup(&self, node: u32) -> DerivedLookup<'_> {
        let mut changes = Vec::new();
        for delta in self.deltas.iter().rev() {
            if !delta.derived.is_empty() {
                let list = delta.derived.for_node(node);
                if !list.is_empty() {
                    changes.push(list);
                }
            }
        }
        let derived = &self.frozen.derived;
        for layer in derived.layers.iter().rev() {
            let list = layer.for_node(node);
            if !list.is_empty() {
                changes.push(list);
            }
        }
        DerivedLookup {
            changes,
            root_row: derived.root_ref(&self.base).row(node),
        }
    }

    /// Construct a `ReaderSnapshot` from its constituent parts.
    ///
    /// Used by [`crate::db::GraphDb::reader`] — the only site that builds a
    /// snapshot — so the private `cache` field stays encapsulated here.
    pub(crate) fn new(
        frozen: Arc<FrozenOverlay>,
        base: Option<Arc<MappedBase>>,
        deltas: Vec<Arc<CommitDelta>>,
        version: u64,
        role_masks: Arc<RoleMaskCache>,
    ) -> Self {
        Self {
            frozen,
            base,
            deltas,
            version,
            role_masks,
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
    ///
    /// Memoised per `(role, version)` in the cache shared with the originating
    /// `GraphDb`, so a scoped reader taking snapshot after snapshot between two
    /// writes resolves the role once.
    pub fn mask_for_role(&self, role: &str) -> Result<NodeMask> {
        self.role_masks
            .get_or_build(role, self.version, || {
                mask_for_role_from(self.effective()?, &self.base, role)
            })
            .map(|m| (*m).clone())
    }

    /// Every live node in `namespace`, as a visibility mask.
    ///
    /// The snapshot-reader twin of [`GraphDb::mask_for_namespace`](crate::GraphDb::mask_for_namespace):
    /// read off the effective state's own `ns` column rather than a derived
    /// array, exactly as the namespace leg of [`Self::mask_for_role`] is. A name
    /// no node uses gives an empty mask — a namespace scope never widens.
    pub fn mask_for_namespace(&self, namespace: &str) -> Result<NodeMask> {
        let state = self.effective()?;
        let cv = build_cv(&state.props, &self.base);
        let mut visible = HashSet::new();
        for (i, &sym) in state.labels.iter().enumerate() {
            if sym == u32::MAX {
                continue; // tombstoned: the label sentinel is what marks it gone
            }
            let id = i as u32;
            let value = cv.get(id, core_storage::NS_PROP).map(|vr| vr.into_value());
            if core_storage::namespace_of_value(value.as_ref()) == namespace {
                visible.insert(id);
            }
        }
        Ok(NodeMask::from_ids(visible))
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

    /// Every directed edge incident on `key`. `derived` is exactly what
    /// [`GraphDb::node_edges`](crate::GraphDb::node_edges) reports: whether a
    /// rule owns the edge.
    ///
    /// Unknown key → `Err(GraphError::KeyNotFound)`.
    pub fn node_edges(&self, key: &str) -> Result<Vec<EdgeInfo>> {
        node_edges_from(key, self.effective()?, &self.base, self)
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

    /// Every edge incident on `key` that the scope may see.
    ///
    /// The snapshot twin of [`GraphDb::node_edges_scoped`](crate::GraphDb::node_edges_scoped),
    /// and the contract HTTP's role-token branch used to write by hand: the
    /// subject is checked first, so a hidden key answers exactly as an absent
    /// one does, and then every edge whose *other* endpoint is hidden is
    /// dropped — a visible node must not become a window onto its hidden
    /// neighbours.
    ///
    /// `derived` is reported as [`Self::node_edges`] reports it.
    ///
    /// Hidden or unknown `key` → [`GraphError::KeyNotFound`].
    pub fn node_edges_scoped(&self, key: &str, mask: &NodeMask) -> Result<Vec<EdgeInfo>> {
        let state = self.effective()?;
        if !state.ids.get(key).is_some_and(|id| mask.contains_id(id)) {
            return Err(GraphError::KeyNotFound { key: key.into() });
        }
        let edges = node_edges_from(key, state, &self.base, self)?;
        Ok(edges
            .into_iter()
            .filter(|e| {
                let other = if e.src_key == key {
                    &e.dst_key
                } else {
                    &e.src_key
                };
                state.ids.get(other).is_some_and(|id| mask.contains_id(id))
            })
            .collect())
    }

    /// BFS expansion from `key`, with the subject check
    /// [`Self::neighborhood_masked`] deliberately omits.
    ///
    /// The snapshot twin of [`GraphDb::neighborhood_scoped`](crate::GraphDb::neighborhood_scoped).
    /// Expansion is unchanged — hidden nodes are neither returned nor traversed
    /// through — so a visible node reachable only through a hidden one stays
    /// out.
    ///
    /// Hidden or unknown `key` → [`GraphError::KeyNotFound`].
    pub fn neighborhood_scoped(
        &self,
        key: &str,
        depth: u32,
        edge_types: Option<&[&str]>,
        dir: Dir,
        mask: &NodeMask,
    ) -> Result<ResultSet> {
        let state = self.effective()?;
        if !state.ids.get(key).is_some_and(|id| mask.contains_id(id)) {
            return Err(GraphError::KeyNotFound { key: key.into() });
        }
        neighborhood_masked_from(key, state, &self.base, depth, edge_types, dir, mask)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })
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

/// [`ReaderSnapshot::derived_lookup`]'s answer for one node.
struct DerivedLookup<'a> {
    /// Newest first.
    changes: Vec<&'a [(DerivedTriple, bool)]>,
    /// The root's triples touching the node.
    root_row: &'a [DerivedTriple],
}

impl DerivedLookup<'_> {
    fn is_derived(&self, triple: DerivedTriple) -> bool {
        for list in &self.changes {
            if let Ok(i) = list.binary_search_by_key(&triple, |&(t, _)| t) {
                return list[i].1;
            }
        }
        row_contains(self.root_row, triple)
    }
}

fn node_edges_from(
    key: &str,
    state: &FrozenOverlay,
    base: &Option<Arc<MappedBase>>,
    snapshot: &ReaderSnapshot,
) -> Result<Vec<EdgeInfo>> {
    let id = state
        .ids
        .get(key)
        .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
    let derived = snapshot.derived_lookup(id);
    let tv = build_tv(&state.topo, base);
    let mut edges = Vec::new();
    for etype in tv.etypes() {
        let edge_type = state
            .syms
            .resolve(etype)
            .ok_or_else(|| GraphError::Corrupt {
                detail: format!("reader: topology etype {etype} not in interner"),
            })?
            .to_string();
        for dir in [Direction::Out, Direction::In] {
            for &nbr in tv.neighbors(etype, dir, id).as_ref() {
                let (src_id, dst_id) = match dir {
                    Direction::Out => (id, nbr),
                    Direction::In => (nbr, id),
                };
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
                    derived: derived.is_derived((etype, src_id, dst_id)),
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
    let start_id = state.ids.get(key)?;
    let view = make_view(state, base, Some(&mask.visible));
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
        let lbl = view
            .label_of(*nid)
            .expect("real nodes always have a label; u32::MAX sentinel cannot occur");
        rs.push_row(vec![
            Some(Value::Str(k.to_string())),
            Some(Value::Str(lbl.to_string())),
            Some(Value::Int(*d as i64)),
        ]);
        visited.push((*nid, *d));
    }
    // Stub mode: add hidden direct neighbours of each visited node as stubs.
    // Hidden nodes are edge-endpoints only — they are not added to the BFS
    // frontier, so the BFS never expands through them in either mode.
    //
    // Role-token callers always pass an Omit-mode mask (mask_for_role uses
    // NodeMask::from_ids which defaults to Omit; intersect() hard-returns Omit),
    // so this branch is unreachable on the role path — security is unaffected.
    if mask.mode() == crate::mask::MaskMode::Stub {
        let raw_view = make_view(state, base, None);
        let mut seen: HashSet<u32> = visited.iter().map(|(id, _)| *id).collect();
        for (node_id, node_depth) in &visited {
            if *node_depth >= depth {
                continue;
            }
            for e in expand(&raw_view, *node_id, resolved.as_deref(), dir) {
                let nbr = if e.src == *node_id { e.dst } else { e.src };
                if !mask.contains_id(nbr) && seen.insert(nbr) {
                    if let Some(k) = state.ids.key_of(nbr) {
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

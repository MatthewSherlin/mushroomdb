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

use core_query::cypher::{execute_union, is_write_tokens, lex, parse_read, Params};
use core_query::{expand, neighborhood, Dir, GraphView, ResultSet};
use core_storage::property_index::PropertyIndex;
use core_storage::v8::seam::{ArchivedNeighbor, DerivedBits};
use core_storage::v8::seam::{ColumnsView, TopologyView};
use core_storage::v8::MappedBase;
use core_storage::wal::WalRecord;
use core_storage::{
    ColumnStore, Direction, EdgeProps, EdgePropsEntry, EdgePropsView, GraphError, IdMap, Interner,
    Result, Topology, Value,
};

use crate::db::{
    call_statement, ddl_statement, explain_answer, run_call, schema_change_on_a_read, EdgeInfo,
    MaskedEdge, MaskedNodeResult, NodeInfo, SchemaListing, Seen, SlowQuerySink, SlowQuerySnapshot,
};
use crate::layer::DeltaLayer;
use crate::mask::{NodeMask, RoleMaskCache};
use crate::roles::RoleDef;

/// Fold trigger: every K commits, the overlay is cloned into a new `FrozenOverlay`
/// and `delta_tail` is reset. The tail length is always ≤ K−1.
pub const FOLD_EVERY_K: usize = 64;

/// Per-commit overlay change record. Immutable after creation.
///
/// `#[non_exhaustive]`: the reader gains fields as it learns to carry more
/// of a commit (0.10 added `edge_props`), so it is built only in this crate.
#[non_exhaustive]
pub struct CommitDelta {
    /// WAL records for this commit (including `Intern` records, in WAL order).
    pub records: Vec<WalRecord>,
    /// Rule-derived edge **inserts** fired this commit: `(etype_sym, src_id, dst_id)`.
    pub derived_inserts: Vec<(u32, u32, u32)>,
    /// Rule-derived edge **retractions** this commit: `(etype_sym, src_id, dst_id)`.
    pub derived_deletes: Vec<(u32, u32, u32)>,
    /// View properties as this commit left them: `(node_id, view_prop, value)`,
    /// `None` where the view has no value for the node. A view's value is in
    /// no record — the writer computes it from the edges and properties the
    /// records change — so the reader is handed the result rather than made
    /// to recompute it.
    pub view_values: Vec<(u32, String, Option<Value>)>,
    /// Edge-property overlay entries as this commit left them, for every edge
    /// whose properties it wrote. A rule writes its weights with no record of
    /// their own, so — as with view values — the reader is handed the result.
    pub edge_props: Vec<((u32, u32, u32), EdgePropsEntry)>,
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
    /// The snapshot base. Its derived-bits section answers for each edge in
    /// place; a snapshot that predates the section is answered from a table
    /// built from its PROVENANCE section by the first read that needs it.
    Base(OnceLock<NodeIndex>),
}

/// A [`DerivedRoot`] resolved against the base it reads.
enum RootRef<'a> {
    Empty,
    Table(&'a NodeIndex),
    Bits(DerivedBits<'a>, &'a MappedBase),
}

impl RootRef<'_> {
    fn contains(&self, triple: DerivedTriple) -> bool {
        match self {
            RootRef::Empty => false,
            RootRef::Table(index) => row_contains(index.row(triple.1), triple),
            RootRef::Bits(bits, base) => base
                .topology()
                .is_ok_and(|csr| bits.edge_is_derived(csr, triple.0, triple.1, triple.2)),
        }
    }
}

/// Multiplicative hash for node ids: they are small integers already spread by
/// allocation order, so SipHash's cost buys nothing here.
#[derive(Default, Clone, Copy)]
struct NodeHasher(u64);

impl std::hash::Hasher for NodeHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 << 8) | u64::from(byte);
        }
    }

    fn write_u32(&mut self, node: u32) {
        self.0 = u64::from(node).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type NodeHashBuilder = std::hash::BuildHasherDefault<NodeHasher>;

/// The derived-edge changes of the unfolded delta tail, in one index: what every
/// commit since the last fold left each triple as, grouped by node, so a read
/// asks for the tail's state with one lookup instead of one per commit.
///
/// The writer appends each commit's changes as the commit is recorded and
/// starts over at a fold, where the changes move into the fold's layers.
#[derive(Default, Clone)]
pub struct TailDerived {
    rows: HashMap<u32, Arc<[(DerivedTriple, bool)]>, NodeHashBuilder>,
    /// One bit per node below [`TAIL_BITMAP_NODES`] that has a row, so a read
    /// of a node the tail never touched — nearly every read — is one load from
    /// a few kilobytes that stay in cache, not a probe of `rows`.
    touched: Vec<u64>,
}

/// Node ids at and above this are not in [`TailDerived::touched`]; they are
/// always looked up in the map.
const TAIL_BITMAP_NODES: u32 = 1 << 20;

impl TailDerived {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Lay one commit's changes over the tail's: a later commit overrides an
    /// earlier one's state of the same triple.
    pub fn append(&mut self, changes: &DerivedChanges) {
        for (i, &node) in changes.nodes.iter().enumerate() {
            let new = &changes.entries[changes.starts[i] as usize..changes.starts[i + 1] as usize];
            let row: Arc<[(DerivedTriple, bool)]> = match self.rows.get(&node) {
                None => Arc::from(new),
                Some(old) => {
                    let mut merged = Vec::with_capacity(old.len() + new.len());
                    let (mut o, mut n) = (0, 0);
                    while o < old.len() && n < new.len() {
                        match old[o].0.cmp(&new[n].0) {
                            std::cmp::Ordering::Less => {
                                merged.push(old[o]);
                                o += 1;
                            }
                            std::cmp::Ordering::Greater => {
                                merged.push(new[n]);
                                n += 1;
                            }
                            std::cmp::Ordering::Equal => {
                                merged.push(new[n]);
                                o += 1;
                                n += 1;
                            }
                        }
                    }
                    merged.extend_from_slice(&old[o..]);
                    merged.extend_from_slice(&new[n..]);
                    Arc::from(merged)
                }
            };
            if node < TAIL_BITMAP_NODES {
                let word = node as usize / 64;
                if self.touched.len() <= word {
                    self.touched.resize(word + 1, 0);
                }
                self.touched[word] |= 1 << (node % 64);
            }
            self.rows.insert(node, row);
        }
    }

    fn row(&self, node: u32) -> &[(DerivedTriple, bool)] {
        if node < TAIL_BITMAP_NODES {
            let untouched = self
                .touched
                .get(node as usize / 64)
                .is_none_or(|word| word >> (node % 64) & 1 == 0);
            if untouched {
                return &[];
            }
        }
        self.rows.get(&node).map_or(&[], |row| row)
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
            DerivedRoot::Index(index) => RootRef::Table(index),
            DerivedRoot::Base(table) => {
                let Some(base) = base else {
                    return RootRef::Empty;
                };
                if let Some(bits) = base.derived_bits() {
                    return RootRef::Bits(bits, base);
                }
                RootRef::Table(table.get_or_init(|| {
                    let bytes = base.provenance_raw_bytes().unwrap_or_default();
                    core_storage::v8::encode::provenance_triples(bytes)
                        .map_or_else(NodeIndex::empty, |triples| NodeIndex::build(&triples))
                }))
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
///
/// It holds no full-text index. A reader answers no full-text search
/// (`GraphDb::search` runs on the writer's index; inside a reader query
/// `textMatches()` reads property values), and sharing the writer's index
/// here made the writer's first full-text write after every fold copy the
/// whole index under the write lock, stalling every reader capture.
///
/// `#[non_exhaustive]` for the same reason as [`CommitDelta`] (0.10 added
/// `prop_index`).
#[derive(Clone)]
#[non_exhaustive]
pub struct FrozenOverlay {
    pub ids: Arc<IdMap>,
    pub syms: Arc<Interner>,
    pub topo: Arc<Topology>,
    pub props: Arc<ColumnStore>,
    pub labels: Arc<Vec<u32>>,
    pub edge_props: Arc<EdgeProps>,
    pub roles: Option<Arc<Vec<RoleDef>>>,
    /// The equality index as it stood at the fold. A snapshot answers an
    /// indexed lookup from it, with the nodes its tail touched re-indexed
    /// over the top (see [`ReaderSnapshot`]'s `index`).
    pub prop_index: Arc<PropertyIndex>,
    /// Which edges in `topo` (and the base under it) a rule derived.
    pub derived: DerivedEdges,
    /// The declared indexes and constraints, for `SHOW`. Current: a commit
    /// that declares or withdraws one folds.
    pub(crate) schema: Arc<SchemaListing>,
}

/// A snapshot's fold, the base under it and its delta-tail layer, if any.
pub(crate) type LayerParts<'a> = (
    &'a FrozenOverlay,
    &'a Option<Arc<MappedBase>>,
    Option<&'a DeltaLayer>,
);

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
    ///
    /// Behind one `Arc`, so handing out another handle on the snapshot — once
    /// per capture — touches one reference count, not one per delta.
    pub deltas: Arc<[Arc<CommitDelta>]>,
    /// The derived-edge changes of `deltas`, indexed by node.
    tail_derived: Arc<TailDerived>,
    /// The store's `commit_seq` when this snapshot was taken — the version key
    /// for the shared role-mask memo. The effective state (frozen + deltas) is
    /// exactly the state the live handle had at this commit.
    pub version: u64,
    /// Role → mask memo, shared with the `GraphDb` this snapshot came from.
    role_masks: Arc<RoleMaskCache>,
    /// `deltas` as one [`DeltaLayer`] over `frozen`: the effective state is
    /// `frozen` read through it.
    ///
    /// Built at most once per `ReaderSnapshot`, on the first call to
    /// [`Self::effective`] when `deltas` is non-empty, at the cost of the
    /// tail's records — nothing below the layer is copied or re-indexed.
    /// Stores `Err(String)` if a record will not apply, so the error is
    /// returned to every later caller without re-attempting. When `deltas` is
    /// empty it is never populated and reads go to `frozen` directly.
    cache: Arc<EffectiveCell>,
    /// What the snapshot knows of the handle beyond its graph state: where
    /// slow queries go, the counts at capture, the WAL's path. Behind one
    /// `Arc`, so another handle on the snapshot copies no path.
    context: Arc<ReadContext>,
    /// The index this snapshot's queries use: the fold's, layered with the
    /// tail's touched nodes; `None` inside when no index applies. Built at
    /// most once, on the first query, and shared like `cache`.
    index: Arc<IndexCell>,
    /// The layer and index of an earlier snapshot whose tail is a prefix of
    /// this one's, for `cache` and `index` to extend instead of rebuild.
    seeds: Arc<Seeds>,
    /// Set when the background builder built this version's cells; shared
    /// like `cache`.
    background: Arc<std::sync::atomic::AtomicBool>,
}

/// The latest built cells of one fold, which a later version takes and
/// extends; see [`SharedEffective`]. Only built cells are registered.
#[derive(Default)]
pub(crate) struct Seeds {
    layer: std::sync::Mutex<Option<(Arc<EffectiveCell>, usize)>>,
    index: std::sync::Mutex<Option<(Arc<IndexCell>, usize)>>,
}

/// Offer a built `cell` for `len` deltas to the fold's register, keeping
/// whichever of it and the registered one covers more of the tail.
fn register<T>(slot: &std::sync::Mutex<Option<(Arc<T>, usize)>>, cell: &Arc<T>, len: usize) {
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_none_or(|(_, have)| *have < len) {
        *slot = Some((Arc::clone(cell), len));
    }
}

/// The value a built cell holds — by move when no one else holds the cell,
/// by clone otherwise — or `None` when it was never built.
fn take_cell<T: Clone>(cell: Arc<OnceLock<T>>) -> Option<T> {
    match Arc::try_unwrap(cell) {
        Ok(once) => once.into_inner(),
        Err(shared) => shared.get().cloned(),
    }
}

/// What a [`ReaderSnapshot`] carries of its handle beyond the graph.
#[derive(Clone)]
pub(crate) struct ReadContext {
    /// Where [`ReaderSnapshot::query`] reports a slow query: the handle's
    /// threshold and ring, so `/metrics` sees reader queries too.
    pub(crate) slow: SlowQuerySink,
    /// `(nodes_live, nodes_tombstoned, edges)` as `GraphDb::stats` counted
    /// them at capture — this snapshot's version — from the handle's own
    /// exact counters.
    pub(crate) counts: (usize, usize, u64),
    /// The WAL file, for [`ReaderSnapshot::wal_size_bytes`].
    pub(crate) wal_path: Option<std::path::PathBuf>,
}

thread_local! {
    /// How many layer and index cells this thread has built: what lets
    /// [`ReaderSnapshot::prepare`] tell whether its call did the build.
    static BUILDS_HERE: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

fn count_build(layer: bool) {
    BUILDS_HERE.with(|c| {
        let (l, i) = c.get();
        c.set(if layer { (l + 1, i) } else { (l, i + 1) });
    });
}

/// A snapshot's delta-tail layer, built at most once.
type EffectiveCell = OnceLock<std::result::Result<DeltaLayer, String>>;

/// A snapshot's query index, built at most once. See [`ReaderSnapshot::query_index`].
type IndexCell = OnceLock<Option<PropertyIndex>>;

/// The layer and index every snapshot of one fold and delta tail shares.
///
/// A snapshot's effective state is a pure function of its fold, its base and
/// its delta tail. Between two commits every snapshot taken has the same
/// three, so they share one [`EffectiveCell`] (and one [`IndexCell`]) instead
/// of each building the tail's layer again — which, with a snapshot per HTTP
/// request, would be once per request.
///
/// And because the tail only grows between folds, a version's cells start
/// from the longest cell already built for the same fold (`seeds`, one
/// register per fold): the layer is extended by the new deltas' records and
/// the index layer re-indexes only the nodes they touched, so a commit costs
/// readers its own records, not the whole tail. Without this, a bulk commit
/// (an ingest batch) was re-applied by every version until the next fold.
///
/// The entry holds the fold, base and last delta it was keyed on, not their
/// addresses alone, so none of them can be freed and its address reused while
/// the entry could still match it.
///
/// # Memory ceiling
///
/// The slot keeps the current version's cells; the register keeps the latest
/// built ones until a later version takes them. A seed is taken by move when
/// nothing else holds it, so the layer is not copied; when an older snapshot
/// still holds it, it is cloned (the size of the tail's changes). Nothing
/// chains: a cell holds no seed.
pub(crate) struct SharedEffective {
    frozen: Arc<FrozenOverlay>,
    base: Option<Arc<MappedBase>>,
    len: usize,
    last: Option<Arc<CommitDelta>>,
    cell: Arc<EffectiveCell>,
    index: Arc<IndexCell>,
    seeds: Arc<Seeds>,
    background: Arc<std::sync::atomic::AtomicBool>,
}

/// Everything [`ReaderSnapshot::new`] builds a snapshot from.
pub(crate) struct SnapshotParts {
    pub(crate) frozen: Arc<FrozenOverlay>,
    pub(crate) base: Option<Arc<MappedBase>>,
    pub(crate) deltas: Arc<[Arc<CommitDelta>]>,
    pub(crate) tail_derived: Arc<TailDerived>,
    pub(crate) version: u64,
    pub(crate) role_masks: Arc<RoleMaskCache>,
    pub(crate) context: ReadContext,
    pub(crate) shared: SharedCell,
}

/// What [`SharedEffective::cell_for`] hands a new snapshot.
pub(crate) struct SharedCell {
    cell: Arc<EffectiveCell>,
    index: Arc<IndexCell>,
    seeds: Arc<Seeds>,
    background: Arc<std::sync::atomic::AtomicBool>,
}

fn same_arc<T>(a: Option<&Arc<T>>, b: Option<&Arc<T>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

impl SharedEffective {
    /// The cells for `(frozen, base, deltas)`, reusing the ones in `slot` when
    /// they were made for exactly that state and replacing them otherwise.
    ///
    /// The delta tail only grows between folds and a fold replaces `frozen`,
    /// so the fold, the tail's length and its last entry identify the tail.
    pub(crate) fn cell_for(
        slot: &std::sync::Mutex<Option<SharedEffective>>,
        frozen: &Arc<FrozenOverlay>,
        base: &Option<Arc<MappedBase>>,
        deltas: &[Arc<CommitDelta>],
    ) -> SharedCell {
        let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        let mut seeds = None;
        if let Some(entry) = slot.as_ref() {
            let same_fold =
                Arc::ptr_eq(&entry.frozen, frozen) && same_arc(entry.base.as_ref(), base.as_ref());
            if same_fold
                && entry.len == deltas.len()
                && same_arc(entry.last.as_ref(), deltas.last())
            {
                return SharedCell {
                    cell: Arc::clone(&entry.cell),
                    index: Arc::clone(&entry.index),
                    seeds: Arc::clone(&entry.seeds),
                    background: Arc::clone(&entry.background),
                };
            }
            // One register per fold: every tail of a fold extends every
            // shorter one, so a built cell of any earlier version seeds this.
            if same_fold {
                seeds = Some(Arc::clone(&entry.seeds));
            }
        }
        let cell = Arc::new(EffectiveCell::new());
        let index = Arc::new(IndexCell::new());
        let seeds = seeds.unwrap_or_default();
        let background = Arc::new(std::sync::atomic::AtomicBool::new(false));
        *slot = Some(SharedEffective {
            frozen: Arc::clone(frozen),
            base: base.clone(),
            len: deltas.len(),
            last: deltas.last().cloned(),
            cell: Arc::clone(&cell),
            index: Arc::clone(&index),
            seeds: Arc::clone(&seeds),
            background: Arc::clone(&background),
        });
        SharedCell {
            cell,
            index,
            seeds,
            background,
        }
    }
}

// ── Private view-building helpers ─────────────────────────────────────────────

pub(crate) fn build_tv<'a>(
    topo: &'a Topology,
    base: &'a Option<Arc<MappedBase>>,
) -> TopologyView<'a> {
    match base {
        None => TopologyView::owned(topo),
        Some(b) => {
            let csr = b.topology().expect("base CSR CRC already verified at open");
            TopologyView::with_base(topo, csr)
        }
    }
}

pub(crate) fn build_cv<'a>(
    props: &'a ColumnStore,
    base: &'a Option<Arc<MappedBase>>,
) -> ColumnsView<'a> {
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

pub(crate) fn build_epv<'a>(
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
    layer: Option<&'a DeltaLayer>,
    mask: Option<&'a core_query::visible::VisibleSet>,
) -> GraphView<'a> {
    if let Some(layer) = layer {
        return layer.view(state, base, mask);
    }
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
        layer: None,
    }
}

// ── ReaderSnapshot ────────────────────────────────────────────────────────────

/// Resolve `role` against a snapshot's effective state — the reader-side twin
/// of [`crate::db::GraphDb::mask_for_role`], and kept identical to it.
///
/// Reads through `view` because a role carrying a `visible_where` predicate
/// has to read properties, and a node's property may live in the mmap'd base,
/// the fold's overlay or the delta tail.
fn mask_for_role_from(
    view: &GraphView<'_>,
    roles: &Option<Arc<Vec<RoleDef>>>,
    role: &str,
) -> Result<NodeMask> {
    let roles = roles.as_ref().ok_or_else(|| GraphError::Corrupt {
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
        if let Some(id) = view.node_id(key) {
            visible.insert(id);
        }
    }
    for label_name in &def.labels {
        if let Some(sym) = view.sym(label_name) {
            for (i, s) in view.label_syms().enumerate() {
                if s != sym {
                    continue;
                }
                let id = i as u32;
                match &def.visible_where {
                    Some(pred) => {
                        let value = view.props.get(id, &pred.field).map(|vr| vr.into_value());
                        if pred.holds(value.as_ref()) {
                            visible.insert(id);
                        }
                    }
                    None => {
                        visible.insert(id);
                    }
                }
            }
        }
    }
    // Namespace leg — the live resolver's retain, against the effective
    // state's own `ns` column (the reader has no derived `node_ns` array: its
    // effective state is assembled per snapshot, and reading the column it
    // would mirror is the same answer by construction). Intersects the key
    // leg too; see `RoleDef::namespaces`.
    if def.namespaces.is_some() {
        visible.retain(|&id| {
            let value = view
                .props
                .get(id, core_storage::NS_PROP)
                .map(|vr| vr.into_value());
            def.sees_namespace(core_storage::namespace_of_value(value.as_ref()))
        });
    }
    Ok(NodeMask::from_ids(visible))
}

impl ReaderSnapshot {
    /// What the delta tail, the fold's layers and its root say about which of
    /// `node`'s edges are derived.
    fn derived_lookup(&self, node: u32) -> DerivedLookup<'_> {
        let derived = &self.frozen.derived;
        let mut layers = Vec::new();
        for layer in derived.layers.iter().rev() {
            let list = layer.for_node(node);
            if !list.is_empty() {
                layers.push(list);
            }
        }
        DerivedLookup {
            tail: self.tail_derived.row(node),
            layers,
            root: match derived.root_ref(&self.base) {
                RootRef::Empty => RootRow::Row(&[]),
                RootRef::Table(index) => RootRow::Row(index.row(node)),
                RootRef::Bits(bits, _) => RootRow::Bits(bits),
            },
        }
    }

    /// Construct a `ReaderSnapshot` from its constituent parts.
    ///
    /// Used by [`crate::db::GraphDb::reader`] — the only site that builds a
    /// snapshot — so the private `cache` field stays encapsulated here.
    pub(crate) fn new(parts: SnapshotParts) -> Self {
        let SnapshotParts {
            frozen,
            base,
            deltas,
            tail_derived,
            version,
            role_masks,
            context,
            shared,
        } = parts;
        Self {
            frozen,
            base,
            deltas,
            tail_derived,
            version,
            role_masks,
            cache: shared.cell,
            context: Arc::new(context),
            index: shared.index,
            seeds: shared.seeds,
            background: shared.background,
        }
    }

    /// Another handle on exactly this snapshot: same state, same shared
    /// layer and index.
    pub(crate) fn share(&self) -> ReaderSnapshot {
        ReaderSnapshot {
            frozen: Arc::clone(&self.frozen),
            base: self.base.clone(),
            deltas: Arc::clone(&self.deltas),
            tail_derived: Arc::clone(&self.tail_derived),
            version: self.version,
            role_masks: Arc::clone(&self.role_masks),
            cache: Arc::clone(&self.cache),
            context: Arc::clone(&self.context),
            index: Arc::clone(&self.index),
            seeds: Arc::clone(&self.seeds),
            background: Arc::clone(&self.background),
        }
    }

    /// This snapshot's fold, the base under it and the delta tail's layer —
    /// what a transaction lays its own writes over (it starts from a copy of
    /// the tail's layer, which costs the tail, not the store).
    pub(crate) fn layer_parts(&self) -> Result<LayerParts<'_>> {
        Ok((&self.frozen, &self.base, self.effective()?))
    }

    /// Run `f` over an unmasked [`GraphView`] of this snapshot's effective
    /// state.
    pub(crate) fn with_view<R>(&self, f: impl FnOnce(&GraphView<'_>) -> R) -> Result<R> {
        Ok(f(&self.view(None)?))
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// A view of the effective state: `frozen` with the delta tail's layer
    /// on top.
    fn view<'a>(
        &'a self,
        mask: Option<&'a core_query::visible::VisibleSet>,
    ) -> Result<GraphView<'a>> {
        Ok(make_view(&self.frozen, &self.base, self.effective()?, mask))
    }

    /// The delta tail's layer over `frozen`, `None` when the tail is empty.
    ///
    /// Built from the tail's records exactly once (cached in `self.cache`), so
    /// that all operations within a single `ReaderSnapshot` share it (F3: no
    /// triple build per request). It costs the tail's records; the fold below
    /// it is neither copied nor re-indexed.
    fn effective(&self) -> Result<Option<&DeltaLayer>> {
        if self.deltas.is_empty() {
            return Ok(None);
        }
        let mut built = false;
        let cached = self.cache.get_or_init(|| {
            built = true;
            count_build(true);
            match self.seeded_layer() {
                Some(extended) => extended,
                None => DeltaLayer::from_deltas(&self.frozen, &self.base, &self.deltas),
            }
            .map_err(|e| e.to_string())
        });
        if built && cached.is_ok() {
            register(&self.seeds.layer, &self.cache, self.deltas.len());
        }
        cached
            .as_ref()
            .map(Some)
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
                mask_for_role_from(&self.view(None)?, &self.frozen.roles, role)
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
        let view = self.view(None)?;
        let mut visible = HashSet::new();
        for (i, sym) in view.label_syms().enumerate() {
            if sym == u32::MAX {
                continue; // tombstoned: the label sentinel is what marks it gone
            }
            let id = i as u32;
            let value = view
                .props
                .get(id, core_storage::NS_PROP)
                .map(|vr| vr.into_value());
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
        self.view(None).ok()?.node_id(key)
    }

    /// Execute a read-only Cypher query over the epoch snapshot.
    ///
    /// The snapshot twin of [`GraphDb::query`](crate::GraphDb::query): the same
    /// `UNION` / `UNION ALL` chain, the same stage-prefixed error text
    /// (`lex:` / `parse:` / `execute:`), the same slow-query report into the
    /// originating handle's ring, and equality lookups served from an index.
    pub fn query(&self, cypher: &str, params: &BTreeMap<String, Value>) -> Result<ResultSet> {
        let tokens = lex(cypher).map_err(|e| GraphError::QueryError {
            detail: format!("lex: {e}"),
        })?;
        if let Some(plan) = explain_answer(&tokens, &self.frozen.schema, Seen::All)? {
            return Ok(plan);
        }
        if let Some(stmt) = ddl_statement(&tokens)? {
            if !stmt.is_show() {
                return Err(schema_change_on_a_read());
            }
            return Ok(self.frozen.schema.show(&stmt));
        }
        if let Some(call) = call_statement(&tokens)? {
            return run_call(&call, &self.view(None)?, &self.frozen.schema, params);
        }
        let union = parse_read(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        let mut view = self.view(None)?;
        view.prop_index = self.query_index()?;
        let t0 = std::time::Instant::now();
        let result = execute_union(&view, &union, &Params(params))
            .map(|rs| crate::db::finish_result(&view, rs))
            .map_err(|e| GraphError::QueryError {
                detail: format!("execute: {e}"),
            });
        self.context
            .slow
            .record(t0.elapsed().as_millis() as u64, cypher, self.version);
        result
    }

    /// Execute a read-only Cypher query with a node visibility mask.
    ///
    /// The snapshot twin of [`GraphDb::query_masked`](crate::GraphDb::query_masked),
    /// `UNION` included: every part of the chain runs against the same masked
    /// view.
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
            return Err(GraphError::MaskedReadOnly);
        }
        if let Some(plan) = explain_answer(
            &tokens,
            &self.frozen.schema,
            Seen::Masked(&self.view(None)?, &mask.visible),
        )? {
            return Ok(plan);
        }
        if let Some(stmt) = ddl_statement(&tokens)? {
            let view = self.view(None)?;
            return Ok(self.frozen.schema.show_visible(&stmt, &view, &mask.visible));
        }
        if let Some(call) = call_statement(&tokens)? {
            let view = self.view(Some(&mask.visible))?;
            return run_call(&call, &view, &self.frozen.schema, params);
        }
        let union = parse_read(&tokens).map_err(|e| GraphError::QueryError {
            detail: format!("parse: {e}"),
        })?;
        let mut view = self.view(Some(&mask.visible))?;
        view.prop_index = self.query_index()?;
        execute_union(&view, &union, &Params(params))
            .map(|rs| crate::db::finish_result(&view, rs))
            .map_err(|e| GraphError::QueryError {
                detail: format!("execute: {e}"),
            })
    }

    /// The equality index this snapshot's queries use.
    ///
    /// With no tail it is the fold's index itself. With a tail it is the
    /// fold's index layered with the nodes the tail touched, re-indexed from
    /// their current values — exact without copying the index per commit
    /// (see [`PropertyIndex::layered_over`]). `None` when nothing is indexed,
    /// or the tail declares or drops an index (the writer folds on such a
    /// commit, so this is a safeguard); a query then scans, which answers the
    /// same.
    fn query_index(&self) -> Result<Option<&PropertyIndex>> {
        let frozen = &self.frozen.prop_index;
        if frozen.enabled_pairs().next().is_none() {
            return Ok(None);
        }
        if self.deltas.is_empty() {
            return Ok(Some(frozen));
        }
        let view = self.view(None)?;
        let mut built = false;
        let index = self.index.get_or_init(|| {
            built = true;
            count_build(false);
            self.seeded_index(&view)
                .unwrap_or_else(|| self.layered_index(&view))
        });
        if built && index.is_some() {
            register(&self.seeds.index, &self.index, self.deltas.len());
        }
        Ok(index.as_ref())
    }

    /// The seed's layer extended by the deltas after it, or `None` when there
    /// is no usable seed (the caller then builds the layer from the tail).
    fn seeded_layer(&self) -> Option<Result<DeltaLayer>> {
        let (cell, len) = self
            .seeds
            .layer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        if len > self.deltas.len() {
            register(&self.seeds.layer, &cell, len);
            return None;
        }
        let mut layer = take_cell(cell)?.ok()?;
        Some(
            layer
                .extend(&self.frozen, &self.base, &self.deltas[len..])
                .map(|()| layer),
        )
    }

    /// The seed's index layer with the nodes the later deltas touched
    /// re-indexed, or `None` when there is no usable seed.
    fn seeded_index(&self, view: &GraphView<'_>) -> Option<Option<PropertyIndex>> {
        let (cell, len) = self
            .seeds
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        if len > self.deltas.len() {
            register(&self.seeds.index, &cell, len);
            return None;
        }
        let mut layer = take_cell(cell)??;
        let mut stale = std::collections::BTreeSet::new();
        let mut unresolved = false;
        for delta in &self.deltas[len..] {
            for rec in &delta.records {
                if !touched_nodes(rec, &self.frozen.ids, view, &mut stale, &mut unresolved) {
                    return Some(None);
                }
            }
            stale.extend(delta.view_values.iter().map(|(node, _, _)| *node));
        }
        if unresolved {
            return None;
        }
        let pairs: Vec<(String, String)> =
            self.frozen.prop_index.enabled_pairs().cloned().collect();
        for id in stale {
            layer.restale(id);
            reindex(&mut layer, &pairs, view, id);
        }
        Some(Some(layer))
    }

    /// Whether this snapshot's delta-tail layer, and its query index when one
    /// applies, are already built — what a reader finds after the writer
    /// published it.
    #[doc(hidden)]
    pub fn layer_and_index_built(&self) -> bool {
        let layer = self.deltas.is_empty() || self.cache.get().is_some();
        let index = self.deltas.is_empty()
            || self.frozen.prop_index.enabled_pairs().next().is_none()
            || self.index.get().is_some();
        layer && index
    }

    /// Build this snapshot's delta-tail layer and query index now, into the
    /// cells it shares with every snapshot of its version. The writer calls it
    /// as it publishes, so readers find both built. A tail that will not apply
    /// is left for the reader to report.
    /// Returns whether this call built every cell that needed building (a
    /// cell another thread already built does not count).
    pub(crate) fn prepare(&self) -> bool {
        let (l0, i0) = BUILDS_HERE.with(|c| c.get());
        if self.effective().is_err() {
            return false;
        }
        let needs_index = self.query_index().ok().flatten().is_some() && !self.deltas.is_empty();
        let (l1, i1) = BUILDS_HERE.with(|c| c.get());
        let layer_ok = self.deltas.is_empty() || l1 > l0;
        let index_ok = !needs_index || i1 > i0;
        layer_ok && index_ok
    }

    /// Mark this version's cells as built by the background builder.
    pub(crate) fn mark_built_in_background(&self) {
        self.background
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether the background builder built this version's cells — what a
    /// test checks to know it is reading builder-built cells.
    #[doc(hidden)]
    pub fn built_in_background(&self) -> bool {
        self.background.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether this snapshot's queries answer equality lookups from an index
    /// (the fold's, or the fold's layered with the tail) rather than a scan.
    #[doc(hidden)]
    pub fn queries_use_index(&self) -> bool {
        self.query_index().ok().flatten().is_some()
    }

    /// Build the layered index [`Self::query_index`] serves over a tail;
    /// `view` is this snapshot's unmasked effective state.
    fn layered_index(&self, view: &GraphView<'_>) -> Option<PropertyIndex> {
        let frozen = &self.frozen.prop_index;
        let mut stale = std::collections::BTreeSet::new();
        let mut unresolved = false;
        for delta in self.deltas.iter() {
            for rec in &delta.records {
                if !touched_nodes(rec, &self.frozen.ids, view, &mut stale, &mut unresolved) {
                    return None;
                }
            }
            stale.extend(delta.view_values.iter().map(|(node, _, _)| *node));
        }
        if unresolved {
            // A key neither state can name (renamed, then deleted, inside the
            // tail): find every node whose liveness or label moved instead.
            for id in 0..view.id_bound() {
                let below = self
                    .frozen
                    .labels
                    .get(id as usize)
                    .copied()
                    .filter(|&s| s != u32::MAX);
                if below != view.label_sym(id) {
                    stale.insert(id);
                }
            }
        }
        let pairs: Vec<(String, String)> = frozen.enabled_pairs().cloned().collect();
        let mut layer = PropertyIndex::layered_over(Arc::clone(frozen), stale.clone());
        for &id in &stale {
            reindex(&mut layer, &pairs, view, id);
        }
        Some(layer)
    }

    /// Materialise this snapshot's effective state now, returning the error
    /// the delta tail raises if it cannot be applied.
    ///
    /// The `Option`-returning reads ([`Self::node_info`],
    /// [`Self::neighborhood`], [`Self::neighborhood_masked`],
    /// [`Self::node_info_masked`]) answer `None` for an unreadable tail exactly
    /// as for an absent key. A caller that must tell a damaged store from a
    /// missing node calls this first: once it returns `Ok`, the state is cached
    /// and those reads can no longer fail.
    pub fn ensure_readable(&self) -> Result<()> {
        self.effective().map(|_| ())
    }

    /// `(nodes_live, nodes_tombstoned, edges)` exactly as
    /// [`GraphDb::stats`](crate::GraphDb::stats) counts them, at this
    /// snapshot's version.
    pub fn counts(&self) -> (usize, usize, u64) {
        self.context.counts
    }

    /// The commit sequence this snapshot reads, as
    /// [`GraphDb::commit_seq`](crate::GraphDb::commit_seq) reported it.
    pub fn commit_seq(&self) -> u64 {
        self.version
    }

    /// The WAL file's size on disk now, as
    /// [`GraphDb::wal_size_bytes`](crate::GraphDb::wal_size_bytes) reads it.
    pub fn wal_size_bytes(&self) -> std::io::Result<u64> {
        let path = self.context.wal_path.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "wal_path not available for this Fs implementation",
            )
        })?;
        Ok(std::fs::metadata(path)?.len())
    }

    /// The handle's slow-query ring, as
    /// [`GraphDb::slow_query_snapshot`](crate::GraphDb::slow_query_snapshot)
    /// reports it.
    pub fn slow_query_snapshot(&self) -> SlowQuerySnapshot {
        self.context.slow.snapshot()
    }

    /// PageRank over this snapshot: the twin of
    /// [`GraphDb::pagerank`](crate::GraphDb::pagerank).
    pub fn pagerank(
        &self,
        config: &crate::algo::PageRankConfig,
    ) -> Result<crate::algo::PageRankReport> {
        Ok(crate::algo::pagerank(&self.view(None)?, config))
    }

    /// Weakly-connected components over this snapshot: the twin of
    /// [`GraphDb::connected_components`](crate::GraphDb::connected_components).
    pub fn connected_components(
        &self,
        config: &crate::algo::WccConfig,
    ) -> Result<crate::algo::WccReport> {
        Ok(crate::algo::wcc(&self.view(None)?, config))
    }

    /// Degree centrality over this snapshot: the twin of
    /// [`GraphDb::degree_centrality`](crate::GraphDb::degree_centrality).
    pub fn degree_centrality(
        &self,
        config: &crate::algo::DegreeConfig,
    ) -> Result<crate::algo::DegreeReport> {
        Ok(crate::algo::degree_centrality(&self.view(None)?, config))
    }

    /// Live node info from the epoch snapshot. `None` if key is absent or tombstoned.
    pub fn node_info(&self, key: &str) -> Option<NodeInfo> {
        node_info_from(key, &self.view(None).ok()?)
    }

    /// Every directed edge incident on `key`. `derived` is exactly what
    /// [`GraphDb::node_edges`](crate::GraphDb::node_edges) reports: whether a
    /// rule owns the edge.
    ///
    /// Unknown key → `Err(GraphError::KeyNotFound)`.
    pub fn node_edges(&self, key: &str) -> Result<Vec<EdgeInfo>> {
        node_edges_from(key, &self.view(None)?, self)
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
        neighborhood_masked_from(key, self, depth, edge_types, dir, mask)
    }

    /// Unmasked BFS expansion from `key`: the snapshot twin of
    /// [`NodeRef::neighborhood`](crate::NodeRef::neighborhood). Columns
    /// `["key", "label", "depth"]`, BFS order, the start node excluded.
    ///
    /// Returns `None` when `key` does not exist.
    pub fn neighborhood(
        &self,
        key: &str,
        depth: u32,
        edge_types: Option<&[&str]>,
        dir: Dir,
    ) -> Option<ResultSet> {
        let view = self.view(None).ok()?;
        let start_id = view.node_id(key)?;
        let resolved: Option<Vec<u32>> =
            edge_types.map(|names| names.iter().filter_map(|name| view.sym(name)).collect());
        let nb = neighborhood(&view, start_id, depth, resolved.as_deref(), dir);
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

    /// Mask-aware node lookup: the snapshot twin of
    /// [`GraphDb::node_info_masked`](crate::GraphDb::node_info_masked).
    ///
    /// Unknown key → `None`; visible → `Visible`; hidden → `None` under
    /// [`MaskMode::Omit`](crate::MaskMode::Omit), `Restricted` under
    /// [`MaskMode::Stub`](crate::MaskMode::Stub).
    ///
    /// **SECURITY**: only for client-mask (full-token) callers; a scoped
    /// caller must never be shown a stub.
    pub fn node_info_masked(&self, key: &str, mask: &NodeMask) -> Option<MaskedNodeResult> {
        let view = self.view(None).ok()?;
        let id = view.node_id(key)?;
        if mask.contains_id(id) {
            Some(MaskedNodeResult::Visible(node_info_from(key, &view)?))
        } else {
            match mask.mode() {
                crate::mask::MaskMode::Stub => Some(MaskedNodeResult::Restricted),
                crate::mask::MaskMode::Omit => None,
            }
        }
    }

    /// Edges of `key` with mask-aware hidden-endpoint handling: the snapshot
    /// twin of [`GraphDb::node_edges_masked`](crate::GraphDb::node_edges_masked).
    ///
    /// Omit mode drops every edge whose other endpoint is hidden; Stub mode
    /// keeps it and flags that endpoint restricted. `derived` is reported as
    /// [`Self::node_edges`] reports it.
    ///
    /// Unknown key → [`GraphError::KeyNotFound`].
    ///
    /// **SECURITY**: only for client-mask (full-token) callers.
    pub fn node_edges_masked(&self, key: &str, mask: &NodeMask) -> Result<Vec<MaskedEdge>> {
        let view = self.view(None)?;
        let edges = node_edges_from(key, &view, self)?;
        let hidden = |k: &str| !view.node_id(k).is_some_and(|id| mask.contains_id(id));
        let mut out = Vec::with_capacity(edges.len());
        for e in edges {
            // The subject's own side is never flagged, as the live twin walks
            // each edge from the subject outward; a self-loop is an out-edge.
            let (src_restricted, dst_restricted) = if e.src_key == key {
                (false, hidden(&e.dst_key))
            } else {
                (hidden(&e.src_key), false)
            };
            if (src_restricted || dst_restricted) && mask.mode() == crate::mask::MaskMode::Omit {
                continue;
            }
            out.push(MaskedEdge {
                edge_type: e.edge_type,
                src_key: e.src_key,
                src_restricted,
                dst_key: e.dst_key,
                dst_restricted,
                derived: e.derived,
            });
        }
        Ok(out)
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
        let view = self.view(None)?;
        if !view.node_id(key).is_some_and(|id| mask.contains_id(id)) {
            return Err(GraphError::KeyNotFound { key: key.into() });
        }
        let edges = node_edges_from(key, &view, self)?;
        Ok(edges
            .into_iter()
            .filter(|e| {
                let other = if e.src_key == key {
                    &e.dst_key
                } else {
                    &e.src_key
                };
                view.node_id(other).is_some_and(|id| mask.contains_id(id))
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
        if !self
            .view(None)?
            .node_id(key)
            .is_some_and(|id| mask.contains_id(id))
        {
            return Err(GraphError::KeyNotFound { key: key.into() });
        }
        neighborhood_masked_from(key, self, depth, edge_types, dir, mask)
            .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })
    }
}

/// Write node `id`'s current indexed values, read through `view`, into `layer`.
fn reindex(layer: &mut PropertyIndex, pairs: &[(String, String)], view: &GraphView<'_>, id: u32) {
    let Some(label) = view.label_of(id) else {
        return;
    };
    for (l, field) in pairs {
        if l == label {
            if let Some(vr) = view.props.get(id, field) {
                layer.set(l, field, id, &vr.into_value());
            }
        }
    }
}

/// Add to `stale` every node `rec` may have changed an indexed value of,
/// resolving keys against both the fold's ids and the effective view. Sets
/// `*unresolved` for a deletion neither can name. Returns `false` when `rec`
/// declares or drops an index, which no layer over the fold can express.
fn touched_nodes(
    rec: &WalRecord,
    frozen: &IdMap,
    effective: &GraphView<'_>,
    stale: &mut std::collections::BTreeSet<u32>,
    unresolved: &mut bool,
) -> bool {
    let by_key = |key: &str, stale: &mut std::collections::BTreeSet<u32>| {
        let a = frozen.get(key);
        let b = effective.node_id(key);
        stale.extend(a);
        stale.extend(b);
        a.is_some() || b.is_some()
    };
    match rec {
        // A declaration folds (see `declares_schema`), so a tail holding one
        // is a safeguard case: no layered index is built over it.
        rec if crate::db::is_declaration(rec) => return false,
        WalRecord::InsertNode { key, .. }
        | WalRecord::InsertNodeId { key, .. }
        | WalRecord::SetProp { key, .. }
        | WalRecord::RemoveProp { key, .. } => {
            by_key(key, stale);
        }
        WalRecord::DeleteNode { key } => {
            if !by_key(key, stale) {
                *unresolved = true;
            }
        }
        WalRecord::SetPropId { id, .. } => {
            stale.insert(*id);
        }
        WalRecord::Batch(inner) => {
            for r in inner {
                if !touched_nodes(r, frozen, effective, stale, unresolved) {
                    return false;
                }
            }
        }
        _ => {}
    }
    true
}

// ── Free-standing helpers that read through a view ───────────────────────────

fn node_info_from(key: &str, view: &GraphView<'_>) -> Option<NodeInfo> {
    let id = view.node_id(key)?;
    let label = view.label_of(id)?.to_string();
    let mut props = BTreeMap::new();
    for field in view.props.field_names() {
        if let Some(vr) = view.props.get(id, &field) {
            props.insert(field, vr.into_value());
        }
    }
    Some(NodeInfo {
        key: key.to_string(),
        label,
        props,
    })
}

/// How the fold's root answers for one node's edges.
enum RootRow<'a> {
    /// The derived triples the root holds for the node.
    Row(&'a [DerivedTriple]),
    /// The snapshot's derived bits, read at each edge's own place in its row.
    Bits(DerivedBits<'a>),
}

/// [`ReaderSnapshot::derived_lookup`]'s answer for one node.
struct DerivedLookup<'a> {
    /// The unfolded tail's changes touching the node.
    tail: &'a [(DerivedTriple, bool)],
    /// The fold's layers' changes, newest first.
    layers: Vec<&'a [(DerivedTriple, bool)]>,
    root: RootRow<'a>,
}

impl DerivedLookup<'_> {
    /// `slot` is the edge's entry in the base CSR row it came from, when it
    /// came from one: where the snapshot's derived bits say whether it is
    /// derived.
    fn is_derived(&self, triple: DerivedTriple, slot: Option<&ArchivedNeighbor>) -> bool {
        if let Ok(i) = self.tail.binary_search_by_key(&triple, |&(t, _)| t) {
            return self.tail[i].1;
        }
        for list in &self.layers {
            if let Ok(i) = list.binary_search_by_key(&triple, |&(t, _)| t) {
                return list[i].1;
            }
        }
        match &self.root {
            RootRow::Row(row) => row_contains(row, triple),
            RootRow::Bits(bits) => slot.is_some_and(|slot| bits.is_derived(slot)),
        }
    }
}

fn node_edges_from(
    key: &str,
    view: &GraphView<'_>,
    snapshot: &ReaderSnapshot,
) -> Result<Vec<EdgeInfo>> {
    let id = view
        .node_id(key)
        .ok_or_else(|| GraphError::KeyNotFound { key: key.into() })?;
    let derived = snapshot.derived_lookup(id);
    let tv = &view.topo;
    let mut edges = Vec::new();
    for etype in tv.etypes() {
        let edge_type = view
            .sym_name(etype)
            .ok_or_else(|| GraphError::Corrupt {
                detail: format!("reader: topology etype {etype} not in interner"),
            })?
            .to_string();
        for dir in [Direction::Out, Direction::In] {
            let (neighbors, base_row) = tv.neighbors_with_base_row(etype, dir, id);
            // Both lists ascend, so each neighbour's entry in the base row is
            // found by walking the row once alongside.
            let mut row_at = 0;
            for &nbr in neighbors.as_ref() {
                let slot = match (&derived.root, base_row) {
                    (RootRow::Bits(_), Some(row)) => {
                        while row_at < row.len() && u32::from(row[row_at]) < nbr {
                            row_at += 1;
                        }
                        row.get(row_at).filter(|entry| u32::from(**entry) == nbr)
                    }
                    _ => None,
                };
                let (src_id, dst_id) = match dir {
                    Direction::Out => (id, nbr),
                    Direction::In => (nbr, id),
                };
                let (src_key, dst_key) = match dir {
                    Direction::Out => (
                        key.to_string(),
                        view.try_key_of(nbr)
                            .ok_or_else(|| GraphError::Corrupt {
                                detail: format!("topology id {nbr} has no key"),
                            })?
                            .to_string(),
                    ),
                    Direction::In => (
                        view.try_key_of(nbr)
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
                    derived: derived.is_derived((etype, src_id, dst_id), slot),
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
    snapshot: &ReaderSnapshot,
    depth: u32,
    edge_types: Option<&[&str]>,
    dir: Dir,
    mask: &NodeMask,
) -> Option<ResultSet> {
    let raw_view = snapshot.view(None).ok()?;
    let start_id = raw_view.node_id(key)?;
    let view = snapshot.view(Some(&mask.visible)).ok()?;
    let resolved: Option<Vec<u32>> =
        edge_types.map(|names| names.iter().filter_map(|name| view.sym(name)).collect());
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
        let mut seen: HashSet<u32> = visited.iter().map(|(id, _)| *id).collect();
        for (node_id, node_depth) in &visited {
            if *node_depth >= depth {
                continue;
            }
            for e in expand(&raw_view, *node_id, resolved.as_deref(), dir) {
                let nbr = if e.src == *node_id { e.dst } else { e.src };
                if !mask.contains_id(nbr) && seen.insert(nbr) {
                    if let Some(k) = raw_view.try_key_of(nbr) {
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

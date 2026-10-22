//! View seams: overlay-over-base read paths for topology and columns.
//!
//! `TopologyView` is wired into `GraphView.topo` so all topology reads
//! transparently consult overlay then base.  `ColumnsView` is wired into
//! `GraphView.props` (Task 2) so column reads also consult overlay then base.
//!
//! **Overlay/archive asymmetry for vectors**: `ColumnsView::vector()` returns
//! a zero-copy `&[f64]` slice for archived base data only.  Overlay vectors
//! (written after the last snapshot) live as `Value::List([Value::Float, ...])`
//! in the owned `ColumnStore` and are returned by `ColumnsView::get()` as
//! `ValueRef::Owned(Value::List(...))`.  Callers that need vectors from both
//! sources must fall back to `get()` when `vector()` returns `None`.

// The zero-copy f64 transmute in `ColumnsView::vector` is only sound on
// little-endian targets where `rkyv::Archived<f64>` == `rend::F64_le` has the
// same wire representation as `f64`.  Fail at compile time on BE targets.
#[cfg(not(target_endian = "little"))]
compile_error!("core-storage v8 seam: zero-copy f64 transmute requires a little-endian target");

use crate::columns::{ColumnHandle, ColumnStore};
use crate::edge_props::EdgeProps;
use crate::topology::{Direction, Topology};
use crate::types::Value;
use crate::v8::layout::{ArchivedColumnData, ArchivedColumns, ArchivedCsr, ArchivedEdgeProps};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};

// ---------------------------------------------------------------------------
// TopologyView
// ---------------------------------------------------------------------------

/// Overlay-over-base topology view.
///
/// `overlay` holds post-snapshot WAL-replayed edges (always an owned
/// `Topology`). `base` is the zero-copy archived CSR from the V8 snapshot
/// mmap, or `None` when the store was opened from a V5–V7 snapshot.
///
/// All read methods consult `overlay` first, then `base`.  Writes always go
/// through the WAL into `overlay` (never through this view).
pub struct TopologyView<'a> {
    pub overlay: &'a Topology,
    pub base: Option<&'a ArchivedCsr>,
    /// Edge changes laid over `overlay` and `base` together: a reader
    /// snapshot's unfolded delta tail, and an open transaction's writes. Its
    /// adjacency holds the edges inserted, its tombstones the edges deleted
    /// from below. `None` reads `overlay` and `base` alone. See
    /// [`TopologyView::with_layer`].
    pub layer: Option<&'a Topology>,
}

impl<'a> TopologyView<'a> {
    /// Construct a view backed only by owned overlay (no base — V5/V6/V7 path).
    pub fn owned(overlay: &'a Topology) -> Self {
        Self {
            overlay,
            base: None,
            layer: None,
        }
    }

    /// This view with a layer of edge changes on top.
    ///
    /// `tx` is read as one more overlay over everything this view already
    /// reads: an edge in its adjacency is present, an edge in its tombstones
    /// is absent whatever the layers below say. A read costs what it cost
    /// without the layer plus the layer's entries for the vertex asked about —
    /// never anything proportional to the store.
    pub fn with_layer(mut self, tx: &'a Topology) -> Self {
        self.layer = Some(tx);
        self
    }

    /// Construct a view that merges an mmap'd V8 base with a WAL-replay overlay.
    ///
    /// Used when a V8 snapshot is open: the base holds the snapshot CSR and the
    /// overlay holds only post-snapshot WAL mutations.
    pub fn with_base(overlay: &'a Topology, base: &'a ArchivedCsr) -> Self {
        Self {
            overlay,
            base: Some(base),
            layer: None,
        }
    }

    /// Sorted unique neighbors of `v` for `(etype, dir)`.
    ///
    /// When base is present the result is the sorted-unique union of:
    ///   `overlay_neighbors ∪ base_neighbors`
    ///
    /// This is correct for Task 1 (overlay starts empty on V8 open; WAL
    /// replay only adds post-snapshot edges so there is no overlap).
    /// Edge deletions from base are tracked by the overlay; see
    /// `MergedNeighbors` for the planned tombstone subtraction (Task 3).
    pub fn neighbors(&self, etype: u32, dir: Direction, v: u32) -> Cow<'a, [u32]> {
        self.neighbors_with_base_row(etype, dir, v).0
    }

    /// [`Self::neighbors`], and the base's own stored row for the same
    /// `(etype, dir, v)` — before the overlay's tombstones and edges are laid
    /// over it — found by the one lookup, not a second.
    ///
    /// The row is what a reader needs to tell where a base edge sits in the
    /// snapshot (see [`DerivedBits`]).
    pub fn neighbors_with_base_row(
        &self,
        etype: u32,
        dir: Direction,
        v: u32,
    ) -> (Cow<'a, [u32]>, Option<&'a [ArchivedNeighbor]>) {
        let (below, row) = self.below_tx_with_base_row(etype, dir, v);
        match self.layer {
            None => (below, row),
            Some(tx) => (lay_tx_over(below, tx, etype, dir, v), row),
        }
    }

    /// [`Self::neighbors_with_base_row`] of the overlay and base alone,
    /// before the layer.
    fn below_tx_with_base_row(
        &self,
        etype: u32,
        dir: Direction,
        v: u32,
    ) -> (Cow<'a, [u32]>, Option<&'a [ArchivedNeighbor]>) {
        let overlay_nbrs = self.overlay.neighbors(etype, dir, v);
        let base = match self.base {
            None => return (overlay_nbrs, None),
            Some(b) => b,
        };

        // Locate the etype entry in the sorted archived CSR.
        let row = archived_csr_row(base, etype, dir, v).map(|r| r.neighbors.as_slice());
        let Some(row_slice) = row else {
            return (overlay_nbrs, None);
        };
        #[cfg(test)]
        BASE_NEIGHBOR_COLLECTS.with(|c| c.set(c.get() + 1));
        let base_nbrs: Vec<u32> = row_slice.iter().map(|n| u32::from(*n)).collect();

        if base_nbrs.is_empty() {
            return (overlay_nbrs, row);
        }

        // Subtract overlay tombstones from base neighbors before merging.
        // When the overlay records an edge deletion for an edge that exists only
        // in the base CSR, `remove_edge` writes a tombstone into the overlay.
        // That tombstone must be filtered out here so deleted edges do not
        // reappear after a V8 snapshot open.
        let filtered_base_nbrs = subtract_tombstones(base_nbrs, etype, dir, v, self.overlay);

        if filtered_base_nbrs.is_empty() {
            return (overlay_nbrs, row);
        }
        if overlay_nbrs.is_empty() {
            return (Cow::Owned(filtered_base_nbrs), row);
        }
        (
            Cow::Owned(merge_sorted_unique(
                overlay_nbrs.as_ref(),
                &filtered_base_nbrs,
            )),
            row,
        )
    }

    /// Total edge count: (base - tombstones) + overlay.
    ///
    /// Tombstones in the overlay represent edges that were present only in the
    /// base CSR and were subsequently deleted via `remove_edge`.  They must be
    /// subtracted from the base count to avoid phantom edge counts.
    pub fn edge_count(&self) -> u64 {
        let below = self.below_tx_edge_count();
        let Some(tx) = self.layer else {
            return below;
        };
        // Count exactly: an edge counts once whichever layers hold it.
        let lower = TopologyView {
            overlay: self.overlay,
            base: self.base,
            layer: None,
        };
        let has_below = |etype: u32, src: u32, dst: u32| {
            lower.neighbors(etype, Direction::Out, src).contains(&dst)
        };
        let mut count = below;
        // A tombstoned edge from below is gone unless the layer put it back.
        for (etype, src, dsts) in tx_tombstones(tx) {
            for &dst in dsts {
                let put_back = tx.neighbors(etype, Direction::Out, src).contains(&dst);
                if has_below(etype, src, dst) && !put_back {
                    count = count.saturating_sub(1);
                }
            }
        }
        // The layer's own edges count where nothing below already does.
        for (etype, src, dst) in tx.all_edges() {
            if !has_below(etype, src, dst) {
                count += 1;
            }
        }
        count
    }

    /// [`Self::edge_count`] of the overlay and base alone.
    fn below_tx_edge_count(&self) -> u64 {
        let ov = self.overlay.edge_count();
        match self.base {
            None => ov,
            Some(b) => {
                let bv = u64::from(b.edge_count);
                // Count tombstoned base edges (out-direction is canonical).
                let tombstones: u64 = self
                    .overlay
                    .out_tombstones
                    .values()
                    .flat_map(|m| m.values())
                    .map(|s| s.len() as u64)
                    .sum();
                bv.saturating_sub(tombstones) + ov
            }
        }
    }

    /// Unique neighbour count for `(etype, dir, v)`.
    ///
    /// Uses archived CSR row length when the overlay has no delta for `v` and
    /// there are no tombstones for that tuple (no neighbour-id copy). Otherwise
    /// `|overlay ∪ base − tombstones|` — set subtract, not count-subtract.
    pub fn degree(&self, etype: u32, dir: Direction, v: u32) -> usize {
        if self.layer.is_some() {
            return self.neighbors(etype, dir, v).len();
        }
        match self.base {
            None => self.overlay.degree(etype, dir, v),
            Some(base) => {
                if self.overlay.adj_is_empty(etype, dir, v)
                    && self.overlay.tombstones_are_empty(etype, dir, v)
                {
                    archived_row_degree(base, etype, dir, v)
                } else {
                    self.neighbors(etype, dir, v).len()
                }
            }
        }
    }

    /// Edge-type ids present in overlay and/or base, sorted ascending.
    pub fn etypes(&self) -> std::vec::IntoIter<u32> {
        match (self.base, self.layer) {
            (None, None) => {
                // Fast path: collect from overlay (already sorted by Topology::etypes).
                self.overlay.etypes().collect::<Vec<_>>().into_iter()
            }
            (base, tx) => {
                let mut set: BTreeSet<u32> = self.overlay.etypes().collect();
                for et in base.iter().flat_map(|b| b.etypes.iter()) {
                    set.insert(u32::from(et.etype));
                }
                set.extend(tx.into_iter().flat_map(Topology::etypes));
                set.into_iter().collect::<Vec<_>>().into_iter()
            }
        }
    }
}

/// `below` — a vertex's neighbours under a layer — with the
/// layer's tombstones removed and its own edges merged in.
fn lay_tx_over<'a>(
    below: Cow<'a, [u32]>,
    tx: &'a Topology,
    etype: u32,
    dir: Direction,
    v: u32,
) -> Cow<'a, [u32]> {
    let added = tx.neighbors(etype, dir, v);
    if added.is_empty() && tx.tombstones_are_empty(etype, dir, v) {
        return below;
    }
    let kept = subtract_tombstones(below.into_owned(), etype, dir, v, tx);
    if added.is_empty() {
        return Cow::Owned(kept);
    }
    if kept.is_empty() {
        return added;
    }
    Cow::Owned(merge_sorted_unique(&kept, added.as_ref()))
}

/// Every out-direction tombstone in `tx`, as `(etype, src, dsts)`.
fn tx_tombstones(tx: &Topology) -> impl Iterator<Item = (u32, u32, &BTreeSet<u32>)> {
    tx.out_tombstones
        .iter()
        .flat_map(|(&etype, by_src)| by_src.iter().map(move |(&src, dsts)| (etype, src, dsts)))
}

/// A neighbour id as the snapshot stores it.
pub type ArchivedNeighbor = rkyv::Archived<u32>;

/// Look up the archived CSR row for `(etype, dir, v)`.
fn archived_csr_row(
    base: &ArchivedCsr,
    etype: u32,
    dir: Direction,
    v: u32,
) -> Option<&crate::v8::layout::ArchivedCsrRow> {
    let et_pos = base
        .etypes
        .binary_search_by_key(&etype, |e| u32::from(e.etype))
        .ok()?;
    let et_entry = &base.etypes[et_pos];
    let adj = match dir {
        Direction::Out => &et_entry.out_adj,
        Direction::In => &et_entry.in_adj,
    };
    let row_pos = adj
        .rows
        .binary_search_by_key(&v, |r| u32::from(r.vertex))
        .ok()?;
    Some(&adj.rows[row_pos])
}

/// Whether the snapshot's base holds the edge `src -> dst` of `etype`, by a
/// binary search of `src`'s sorted out-row. Overlay tombstones are the
/// caller's to check: this reads the base alone.
pub fn base_has_out_edge(base: &ArchivedCsr, etype: u32, src: u32, dst: u32) -> bool {
    archived_csr_row(base, etype, Direction::Out, src).is_some_and(|row| {
        row.neighbors
            .binary_search_by_key(&dst, |n| u32::from(*n))
            .is_ok()
    })
}

/// Archived neighbour count without collecting ids.
fn archived_row_degree(base: &ArchivedCsr, etype: u32, dir: Direction, v: u32) -> usize {
    archived_csr_row(base, etype, dir, v)
        .map(|row| row.neighbors.len())
        .unwrap_or(0)
}

#[cfg(test)]
thread_local! {
    static BASE_NEIGHBOR_COLLECTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Merge two sorted-unique slices into a sorted-unique Vec.
fn merge_sorted_unique(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut ai = 0;
    let mut bi = 0;
    while ai < a.len() && bi < b.len() {
        match a[ai].cmp(&b[bi]) {
            std::cmp::Ordering::Less => {
                out.push(a[ai]);
                ai += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[bi]);
                bi += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[ai]);
                ai += 1;
                bi += 1;
            }
        }
    }
    out.extend_from_slice(&a[ai..]);
    out.extend_from_slice(&b[bi..]);
    out
}

/// Remove tombstoned neighbors from `nbrs` (already collected from the base CSR).
///
/// Reads the overlay's tombstone maps for `(etype, dir, v)` and filters out any
/// neighbor that appears in the tombstone set.  Returns the original `nbrs` Vec
/// unchanged (zero allocation) when there are no tombstones.
fn subtract_tombstones(
    nbrs: Vec<u32>,
    etype: u32,
    dir: Direction,
    v: u32,
    overlay: &Topology,
) -> Vec<u32> {
    let tombstones = match dir {
        Direction::Out => overlay.out_tombstones_for(etype, v),
        Direction::In => overlay.in_tombstones_for(etype, v),
    };
    match tombstones {
        None => nbrs,
        Some(t) if t.is_empty() => nbrs,
        Some(t) => nbrs.into_iter().filter(|n| !t.contains(n)).collect(),
    }
}

/// Which of a snapshot's edges a rule derived: section 17, one bit per `u32`
/// of the TOPOLOGY section, set for each neighbour entry (in an out-row or an
/// in-row) that is a rule-derived edge. The bit for an entry is indexed by the
/// entry's own byte offset in the topology section divided by four, so a reader
/// holding a row it found for another reason reads the answer where it lies,
/// with no search and nothing built.
pub struct DerivedBits<'a> {
    pub(crate) bits: &'a [u8],
    /// Address of the first byte of the TOPOLOGY section.
    pub(crate) topology_start: usize,
}

impl DerivedBits<'_> {
    /// Whether the row entry `neighbor` — an element of a row of the base CSR
    /// this section was written beside — is a derived edge.
    pub fn is_derived(&self, neighbor: &ArchivedNeighbor) -> bool {
        let offset =
            (neighbor as *const ArchivedNeighbor as usize).wrapping_sub(self.topology_start);
        let slot = offset / 4;
        self.bits
            .get(slot / 8)
            .is_some_and(|byte| byte >> (slot % 8) & 1 == 1)
    }

    /// Whether the base edge `src -> dst` of `etype` is derived, by a search
    /// of `src`'s out-row. `false` for an edge the base does not hold.
    pub fn edge_is_derived(&self, csr: &ArchivedCsr, etype: u32, src: u32, dst: u32) -> bool {
        archived_csr_row(csr, etype, Direction::Out, src).is_some_and(|row| {
            row.neighbors
                .binary_search_by_key(&dst, |n| u32::from(*n))
                .map(|i| self.is_derived(&row.neighbors[i]))
                .unwrap_or(false)
        })
    }
}

/// The topology-section byte offset, divided by four, of each neighbour entry
/// of `csr` that is one of `triples` — in both its out-row and its in-row. What
/// [`DerivedBits`] reads; `csr` must be the archive of the very bytes that
/// start at `topology_start`.
pub(crate) fn derived_slots(
    csr: &ArchivedCsr,
    topology_start: usize,
    triples: impl Iterator<Item = (u32, u32, u32)>,
) -> Vec<usize> {
    let mut by_source: Vec<(u32, u32, u32)> = triples.collect();
    by_source.sort_unstable();
    by_source.dedup();
    // The same edges as (etype, destination, source), for the in-rows.
    let mut by_destination: Vec<(u32, u32, u32)> = by_source
        .iter()
        .map(|&(etype, src, dst)| (etype, dst, src))
        .collect();
    by_destination.sort_unstable();

    let mut slots = Vec::with_capacity(by_source.len() * 2);
    let slot_of = |element: &ArchivedNeighbor| {
        ((element as *const ArchivedNeighbor as usize) - topology_start) / 4
    };
    // Rows and entries both ascend, so each pass is one walk over the rows
    // with the sorted keys alongside: no search per edge.
    for (dir, keys) in [
        (Direction::Out, &by_source),
        (Direction::In, &by_destination),
    ] {
        let mut start = 0;
        while start < keys.len() {
            let etype = keys[start].0;
            let end = start + keys[start..].partition_point(|k| k.0 == etype);
            let group = &keys[start..end];
            start = end;
            let Ok(position) = csr
                .etypes
                .binary_search_by_key(&etype, |e| u32::from(e.etype))
            else {
                continue;
            };
            let entry = &csr.etypes[position];
            let rows = match dir {
                Direction::Out => &entry.out_adj.rows,
                Direction::In => &entry.in_adj.rows,
            };
            let mut row_at = 0;
            let mut neighbor_at = 0;
            let mut current_vertex = None;
            for &(_, vertex, neighbor) in group {
                if current_vertex != Some(vertex) {
                    current_vertex = Some(vertex);
                    neighbor_at = 0;
                    while row_at < rows.len() && u32::from(rows[row_at].vertex) < vertex {
                        row_at += 1;
                    }
                }
                let Some(row) = rows.get(row_at).filter(|r| u32::from(r.vertex) == vertex) else {
                    continue;
                };
                while neighbor_at < row.neighbors.len()
                    && u32::from(row.neighbors[neighbor_at]) < neighbor
                {
                    neighbor_at += 1;
                }
                if neighbor_at < row.neighbors.len()
                    && u32::from(row.neighbors[neighbor_at]) == neighbor
                {
                    slots.push(slot_of(&row.neighbors[neighbor_at]));
                }
            }
        }
    }
    slots
}

/// An iterator over the sorted-unique merged neighbor list.
/// Produced by `TopologyView::neighbors`; callers use `.as_ref()`.
///
/// This type alias is exported to satisfy the Task-1 interface contract.
pub type MergedNeighbors<'a> = Cow<'a, [u32]>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::columns::ColumnStore;
    use crate::idmap::IdMap;
    use crate::interner::Interner;
    use crate::topology::{RemoveEdgeOutcome, Topology};
    use crate::v8::encode::{encode_v8, V8Meta};
    use crate::v8::MappedBase;
    use std::collections::BTreeMap;

    /// A pre-V9 base carries its table inside each `Str` column and no shared
    /// section; the resolution rule must fall back to that copy.
    ///
    /// The encoder cannot write a V8 snapshot any more, so the legacy shape is
    /// built directly: this is the only place the pre-V9 read path is exercised
    /// with real strings (`golden_v8.bin` holds a single Int property).
    #[test]
    fn a_pre_v9_column_resolves_through_its_own_table() {
        use crate::v8::layout::{ColumnData, ColumnsData, FieldEntry};

        let legacy = ColumnsData {
            fields: vec![FieldEntry {
                name: "tag".to_string(),
                col: ColumnData::Str {
                    ids: vec![2, 0, 1],
                    present: vec![0b111],
                    // The whole table, copied into the column: exactly what
                    // every V5-V8 snapshot wrote for every string column.
                    strings: vec!["alpha".into(), "beta".into(), "gamma".into()],
                },
            }],
        };
        let bytes = rkyv::api::high::to_bytes::<rkyv::rancor::Error>(&legacy).expect("rkyv encode");
        let archived =
            rkyv::access::<crate::v8::layout::ArchivedColumnsData, rkyv::rancor::Error>(&bytes)
                .expect("rkyv access");

        let overlay = ColumnStore::new();
        // `with_base` leaves `strings: None` — the pre-V9 case.
        let view = ColumnsView::with_base(&overlay, archived);
        assert!(view.strings.is_none(), "a pre-V9 base has no shared table");
        for (id, want) in [(0u32, "gamma"), (1, "alpha"), (2, "beta")] {
            match view.get(id, "tag") {
                Some(ValueRef::Owned(Value::Str(got))) => assert_eq!(got, want, "node {id}"),
                other => panic!("node {id}: expected Str({want}), got {other:?}"),
            }
        }

        // The materialising path resolves the same way with `shared: None`.
        let store = crate::v8::encode::archived_to_columnstore(archived, None);
        assert_eq!(store.get(0, "tag"), Some(&Value::Str("gamma".into())));
        assert_eq!(store.get(1, "tag"), Some(&Value::Str("alpha".into())));
        assert_eq!(store.get(2, "tag"), Some(&Value::Str("beta".into())));
    }

    fn tiny_meta() -> V8Meta {
        use std::collections::HashMap;
        V8Meta {
            labels: vec![],
            edge_props: crate::edge_props::EdgeProps::new(),
            rule_defs: vec![],
            provenance: BTreeMap::new(),
            rule_tripped: BTreeMap::new(),
            rule_fires: BTreeMap::new(),
            ivf_bytes: Vec::new(),
            view_defs: vec![],
            wal_truncated: false,
            hnsw: BTreeMap::new(),
            last_change: HashMap::new(),
            fulltext_blob: Vec::new(),
            wal_mark: Vec::new(),
            prov_reconciled: false,
            v11: false,
            declarations: Default::default(),
        }
    }

    /// Tombstone subtraction: base CSR has edge A→B for etype E.  The overlay
    /// records a deletion of A→B via `remove_edge` (tombstone).  After merging,
    /// B must NOT appear in `neighbors(E, Out, A)`, and A must NOT appear in
    /// `neighbors(E, In, B)`.
    #[test]
    fn neighbors_with_deletions_subtracts_from_base() {
        // Encode a V8 snapshot containing a single edge A (id=0) → B (id=1).
        let etype = 7u32;
        let a = 0u32;
        let b = 1u32;

        let mut base_topo = Topology::new();
        base_topo.add_edge(etype, a, b);

        let mut ids = IdMap::new();
        ids.get_or_insert("A");
        ids.get_or_insert("B");

        let meta = tiny_meta();
        let mut snap_bytes = Vec::new();
        encode_v8(
            None,
            None,
            None,
            None,
            None,
            &base_topo,
            &ColumnStore::new(),
            &ids,
            &Interner::new(),
            &meta,
            &mut snap_bytes,
        )
        .expect("encode_v8");

        // Map the snapshot bytes to get an ArchivedCsr (zero-copy base).
        let mapped = MappedBase::from_bytes(snap_bytes).expect("from_bytes");
        let archived_csr = mapped.topology().expect("topology section");

        // The overlay starts empty; call remove_edge for the base-only edge.
        // Since the edge is not in the overlay, this records a tombstone.
        let mut overlay = Topology::new();
        let outcome = overlay.remove_edge(etype, a, b);
        assert_eq!(
            outcome,
            RemoveEdgeOutcome::TombstonedBase,
            "edge was base-only, not in overlay"
        );

        // Build the merged view.
        let view = TopologyView {
            layer: None,
            overlay: &overlay,
            base: Some(archived_csr),
        };

        // Out-direction: B must NOT appear.
        let out_nbrs = view.neighbors(etype, Direction::Out, a);
        assert!(
            !out_nbrs.contains(&b),
            "tombstoned edge A→B must not appear in Out neighbors; got {out_nbrs:?}"
        );

        // In-direction: A must NOT appear.
        let in_nbrs = view.neighbors(etype, Direction::In, b);
        assert!(
            !in_nbrs.contains(&a),
            "tombstoned edge A→B must not appear in In neighbors of B; got {in_nbrs:?}"
        );

        // Verify: an un-tombstoned edge is still visible.
        let c = 2u32;
        let mut base_topo2 = Topology::new();
        base_topo2.add_edge(etype, a, b);
        base_topo2.add_edge(etype, a, c);
        let mut snap2 = Vec::new();
        let mut ids2 = IdMap::new();
        ids2.get_or_insert("A");
        ids2.get_or_insert("B");
        ids2.get_or_insert("C");
        encode_v8(
            None,
            None,
            None,
            None,
            None,
            &base_topo2,
            &ColumnStore::new(),
            &ids2,
            &Interner::new(),
            &tiny_meta(),
            &mut snap2,
        )
        .expect("encode_v8 2");
        let mapped2 = MappedBase::from_bytes(snap2).expect("from_bytes 2");
        let archived_csr2 = mapped2.topology().expect("topology section 2");

        let mut overlay2 = Topology::new();
        overlay2.remove_edge(etype, a, b); // only tombstone B
        let view2 = TopologyView {
            layer: None,
            overlay: &overlay2,
            base: Some(archived_csr2),
        };
        let out_nbrs2 = view2.neighbors(etype, Direction::Out, a);
        assert!(
            !out_nbrs2.contains(&b),
            "B must be hidden by tombstone; got {out_nbrs2:?}"
        );
        assert!(
            out_nbrs2.contains(&c),
            "C must still be visible (not tombstoned); got {out_nbrs2:?}"
        );
    }

    fn encode_csr(topo: &Topology, keys: &[&str]) -> (MappedBase, Topology) {
        let mut ids = IdMap::new();
        for k in keys {
            ids.get_or_insert(k);
        }
        let mut snap = Vec::new();
        encode_v8(
            None,
            None,
            None,
            None,
            None,
            topo,
            &ColumnStore::new(),
            &ids,
            &Interner::new(),
            &tiny_meta(),
            &mut snap,
        )
        .expect("encode_v8");
        let mapped = MappedBase::from_bytes(snap).expect("from_bytes");
        (mapped, Topology::new())
    }

    /// Empty overlay + no tombstones: degree is archived row len, no id collect.
    #[test]
    fn degree_archived_csr_len_does_not_collect_ids() {
        let etype = 7u32;
        let a = 0u32;
        let mut base_topo = Topology::new();
        base_topo.add_edge(etype, a, 1);
        base_topo.add_edge(etype, a, 2);
        let (mapped, overlay) = encode_csr(&base_topo, &["A", "B", "C"]);
        let view = TopologyView::with_base(&overlay, mapped.topology().expect("topology"));

        BASE_NEIGHBOR_COLLECTS.with(|c| c.set(0));
        assert_eq!(view.degree(etype, Direction::Out, a), 2);
        assert_eq!(
            BASE_NEIGHBOR_COLLECTS.with(|c| c.get()),
            0,
            "CSR degree path must not collect neighbour ids"
        );
        assert_eq!(view.neighbors(etype, Direction::Out, a).len(), 2);
        assert!(
            BASE_NEIGHBOR_COLLECTS.with(|c| c.get()) > 0,
            "neighbors() still collects archived ids"
        );
    }

    /// Overlay re-insert of a base neighbour must not double-count.
    #[test]
    fn degree_unique_union_does_not_double_count() {
        let etype = 7u32;
        let a = 0u32;
        let mut base_topo = Topology::new();
        base_topo.add_edge(etype, a, 1);
        base_topo.add_edge(etype, a, 2);
        let (mapped, mut overlay) = encode_csr(&base_topo, &["A", "B", "C", "D"]);
        assert!(overlay.add_edge(etype, a, 1), "re-insert of base dest 1");
        assert!(overlay.add_edge(etype, a, 3), "new overlay dest 3");
        let view = TopologyView::with_base(&overlay, mapped.topology().expect("topology"));
        assert_eq!(view.degree(etype, Direction::Out, a), 3);
        assert_eq!(
            view.neighbors(etype, Direction::Out, a).as_ref(),
            &[1, 2, 3]
        );
    }

    /// Tombstone of an id not in the CSR row must not undercount (set subtract).
    #[test]
    fn degree_tombstone_of_missing_nbr_does_not_undercount() {
        let etype = 7u32;
        let a = 0u32;
        let mut base_topo = Topology::new();
        base_topo.add_edge(etype, a, 1);
        base_topo.add_edge(etype, a, 2);
        let (mapped, mut overlay) = encode_csr(&base_topo, &["A", "B", "C"]);
        overlay.remove_edge(etype, a, 99); // not in CSR
        let view = TopologyView::with_base(&overlay, mapped.topology().expect("topology"));
        assert_eq!(
            view.degree(etype, Direction::Out, a),
            2,
            "count-subtract would yield 1; set subtract keeps both CSR neighbours"
        );
    }

    #[test]
    fn degree_overlay_only_matches_topology() {
        let mut overlay = Topology::new();
        overlay.add_edge(1, 0, 1);
        overlay.add_edge(1, 0, 2);
        let view = TopologyView::owned(&overlay);
        assert_eq!(
            view.degree(1, Direction::Out, 0),
            overlay.degree(1, Direction::Out, 0)
        );
        assert_eq!(view.degree(1, Direction::Out, 0), 2);
        assert_eq!(view.degree(1, Direction::In, 1), 1);
        assert_eq!(view.degree(9, Direction::Out, 0), 0);
    }

    /// A layer adds its edges, hides the ones it tombstoned from
    /// below — even one it re-adds and deletes again — and counts each edge
    /// once.
    #[test]
    fn tx_layer_adds_and_hides_edges_over_the_overlay() {
        let mut below = Topology::new();
        below.add_edge(1, 0, 1);
        below.add_edge(1, 0, 2);
        let mut tx = Topology::new();
        tx.add_edge(1, 0, 3);
        tx.remove_edge(1, 0, 1); // tombstone over below
        tx.add_edge(1, 0, 1); // put back
        tx.remove_overlay_edge(1, 0, 1);
        tx.remove_edge(1, 0, 1); // and gone again
        tx.remove_edge(1, 0, 2);
        tx.add_edge(1, 0, 2); // removed then put back: present
        tx.add_edge(7, 4, 5);
        let view = TopologyView::owned(&below).with_layer(&tx);
        assert_eq!(view.neighbors(1, Direction::Out, 0).as_ref(), &[2, 3]);
        assert_eq!(view.neighbors(1, Direction::In, 1).as_ref(), &[] as &[u32]);
        assert_eq!(view.neighbors(1, Direction::In, 2).as_ref(), &[0]);
        assert_eq!(view.degree(1, Direction::Out, 0), 2);
        assert_eq!(view.edge_count(), 3);
        assert_eq!(view.etypes().collect::<Vec<_>>(), vec![1, 7]);
        // Below is untouched.
        let plain = TopologyView::owned(&below);
        assert_eq!(plain.neighbors(1, Direction::Out, 0).as_ref(), &[1, 2]);
    }

    /// Section 17 marks exactly the neighbour entries — in out-rows and in-rows —
    /// of the edges provenance names, and a snapshot with no such edge has none.
    #[test]
    fn derived_bits_mark_exactly_the_owned_edges() {
        use std::collections::BTreeSet;
        let edges: [(u32, u32, u32); 9] = [
            (1, 0, 1),
            (1, 0, 2),
            (1, 1, 2),
            (1, 3, 0),
            (1, 3, 3),
            (2, 0, 1),
            (2, 2, 0),
            (2, 2, 3),
            (7, 5, 6),
        ];
        let owned: BTreeSet<(u32, u32, u32)> = [
            (1, 0, 2),
            (2, 0, 1),
            (1, 3, 0),
            (1, 3, 3),
            (7, 5, 6),
            (9, 1, 1),
        ]
        .into_iter()
        .collect();
        let mut topo = Topology::new();
        for &(etype, src, dst) in &edges {
            topo.add_edge(etype, src, dst);
        }
        let mut ids = IdMap::new();
        for key in ["a", "b", "c", "d", "e", "f", "g"] {
            ids.get_or_insert(key);
        }

        let encode = |provenance: BTreeMap<String, BTreeSet<(u32, u32, u32)>>| {
            let mut meta = tiny_meta();
            meta.provenance = provenance;
            let mut bytes = Vec::new();
            encode_v8(
                None,
                None,
                None,
                None,
                None,
                &topo,
                &ColumnStore::new(),
                &ids,
                &Interner::new(),
                &meta,
                &mut bytes,
            )
            .expect("encode_v8");
            MappedBase::from_bytes(bytes).expect("from_bytes")
        };

        let base = encode(BTreeMap::from([
            ("r1".to_string(), owned.iter().copied().take(3).collect()),
            ("r2".to_string(), owned.iter().copied().skip(3).collect()),
        ]));
        let bits = base.derived_bits().expect("section 17 present");
        let csr = base.topology().unwrap();
        let mut marked = 0;
        for et in csr.etypes.iter() {
            let etype = u32::from(et.etype);
            for (dir, adj) in [(Direction::Out, &et.out_adj), (Direction::In, &et.in_adj)] {
                for row in adj.rows.iter() {
                    for neighbor in row.neighbors.iter() {
                        let (src, dst) = match dir {
                            Direction::Out => (u32::from(row.vertex), u32::from(*neighbor)),
                            Direction::In => (u32::from(*neighbor), u32::from(row.vertex)),
                        };
                        let expected = owned.contains(&(etype, src, dst));
                        assert_eq!(
                            bits.is_derived(neighbor),
                            expected,
                            "{etype} {src}->{dst} {dir:?}"
                        );
                        marked += usize::from(expected);
                    }
                }
            }
        }
        // Five owned edges are in the topology, each in two rows.
        assert_eq!(marked, 10);
        assert!(bits.edge_is_derived(csr, 1, 0, 2));
        assert!(!bits.edge_is_derived(csr, 1, 0, 1));
        assert!(
            !bits.edge_is_derived(csr, 9, 1, 1),
            "an owned edge the topology lacks"
        );
        for (id, name, _, status) in base.verify_integrity() {
            assert!(status.is_ok(), "section {id} {name}");
        }

        // Nothing owned: no section.
        assert!(encode(BTreeMap::new()).derived_bits().is_none());
        let none_in_topology = encode(BTreeMap::from([(
            "r".to_string(),
            BTreeSet::from([(9, 1, 1)]),
        )]));
        assert!(none_in_topology.derived_bits().is_none());
    }
}

// ---------------------------------------------------------------------------
// ColumnsView and ValueRef
// ---------------------------------------------------------------------------

/// A borrowed or materialized column value.
///
/// `Borrowed` is returned when the value comes from the owned overlay
/// (`&'a Value` into the `ColumnStore`).  `Owned` is returned when the value
/// is materialized from the archived base section (e.g. a decoded scalar or a
/// float list reconstructed from a `Vector` column).
#[derive(Debug)]
pub enum ValueRef<'a> {
    Borrowed(&'a Value),
    Owned(Value),
}

impl<'a> ValueRef<'a> {
    /// Convert to an owned `Value`, cloning if borrowed.
    pub fn into_value(self) -> Value {
        match self {
            Self::Borrowed(v) => v.clone(),
            Self::Owned(v) => v,
        }
    }

    /// Borrow the inner value.
    pub fn as_value(&self) -> &Value {
        match self {
            Self::Borrowed(v) => v,
            Self::Owned(v) => v,
        }
    }
}

impl<'a> PartialEq<Value> for ValueRef<'a> {
    fn eq(&self, other: &Value) -> bool {
        self.as_value() == other
    }
}

impl<'a> PartialEq for ValueRef<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.as_value() == other.as_value()
    }
}

/// Memoized decode of `Mixed` base columns, owned by the `MappedBase`.
///
/// Every typed column in a V8 base (`Int`, `Float`, `Bool`, `Str`, `Vector`) is
/// indexed by node id, so reading one node's value is O(1) against the mmap.  A
/// `Mixed` column is not: it is a single bincode blob holding *every* node's
/// value for that field, and `List`- and `Map`-valued properties always land
/// there (see `Column::promote` in `columns.rs`).  Decoding it per lookup makes
/// a scan over N nodes cost N full-column decodes.
///
/// The columns section is immutable for the life of the mapping — post-snapshot
/// writes go to the overlay, which `ColumnsView::get` consults first — so each
/// field is decoded once and reused.  The decoded map is no larger than the
/// `ColumnStore` representation the same data occupies when no snapshot exists,
/// so this trades no more memory than a WAL-only open of the same store.
///
/// A blob that fails to decode memoizes as empty, which reads back as "no value
/// for this field" — the same result the uncached path gives for the same bytes.
#[derive(Default)]
pub struct MixedCache {
    /// An `RwLock` rather than a `Mutex`: after the first read of a field every
    /// later one is a shared-lock hash lookup, so concurrent readers of a
    /// `SharedDb` do not queue behind each other on the common path.
    decoded: std::sync::RwLock<HashMap<String, std::sync::Arc<HashMap<u32, Value>>>>,
}

impl MixedCache {
    /// Return the decoded form of the `Mixed` column `field`, decoding `blob`
    /// on the first call and returning the memo on every later call.
    ///
    /// `blob` must be the bytes of that field's column in the base this cache
    /// belongs to; the mapping is immutable, so the pairing is fixed.
    pub fn get_or_decode(&self, field: &str, blob: &[u8]) -> std::sync::Arc<HashMap<u32, Value>> {
        {
            let guard = self.decoded.read().expect("mixed column cache poisoned");
            if let Some(hit) = guard.get(field) {
                return std::sync::Arc::clone(hit);
            }
        }
        // Decoded with no lock held: a concurrent miss on the same field decodes
        // twice and the first insert wins, which costs a little work but never
        // returns different bytes — the blob cannot change.
        let decoded: std::sync::Arc<HashMap<u32, Value>> =
            std::sync::Arc::new(bincode::deserialize(blob).unwrap_or_default());
        let mut guard = self.decoded.write().expect("mixed column cache poisoned");
        std::sync::Arc::clone(
            guard
                .entry(field.to_string())
                .or_insert_with(|| std::sync::Arc::clone(&decoded)),
        )
    }
}

/// A base snapshot's columns section together with the shared string table
/// that resolves its string ids.
///
/// The two travel as a pair because from V9 on neither is complete alone: the
/// columns hold `sid` indices and only the table holds the vocabulary.
/// `strings` is `None` for a pre-V9 base, whose columns still carry their own
/// copies.
#[derive(Copy, Clone)]
pub struct BaseColumns<'a> {
    pub cols: &'a ArchivedColumns,
    pub strings: Option<&'a crate::v8::layout::ArchivedStringTable>,
}

/// Overlay-over-base column store view.
///
/// Reads consult the owned overlay `ColumnStore` first; if the field/node is
/// absent in the overlay, the archived base section is checked.  Writes always
/// go through the WAL into the owned overlay (never through this view).
///
/// **Column handle**: `ColumnsView::column()` returns an overlay-backed
/// `ColumnHandle`.  Base values are NOT visible through the column handle —
/// only through the `get()` path.  This is acceptable because:
///   (a) base is `None` for V5–V7 stores, which have no mmap'd section at all
///       (their whole snapshot is materialised into the overlay at open);
///   (b) callers that need a fused-scan path with base values should use
///       `get()` directly.
///
/// **String columns come in two shapes.** In a V5–V8 base every
/// `ArchivedColumnData::Str` carries its own full copy of the string table and
/// `strings` below is `None`. From V9 on the table is written once as section
/// 12, every column's own copy is empty, and `strings` holds it. `get()`
/// resolves with one rule: if the shared table is present it is the table;
/// otherwise the column's own is.
#[derive(Copy, Clone)]
pub struct ColumnsView<'a> {
    pub overlay: &'a ColumnStore,
    pub base: Option<&'a crate::v8::layout::ArchivedColumns>,
    /// Memo for `Mixed` base columns; see [`MixedCache`].  `None` means every
    /// `Mixed` read decodes the whole column, which is correct but O(column).
    pub mixed: Option<&'a MixedCache>,
    /// The snapshot's one shared string table (section 12).  `None` for a
    /// pre-V9 snapshot, where the column's own `strings` is authoritative.
    pub strings: Option<&'a crate::v8::layout::ArchivedStringTable>,
    /// Property writes of a reader snapshot's unfolded delta tail and an
    /// open transaction, read before everything else: a value there wins, a
    /// removal there hides what lies below. `None` reads the rest alone. See
    /// [`ColumnsView::with_layer`].
    pub layer: Option<&'a PropLayer>,
}

impl<'a> ColumnsView<'a> {
    /// This view with a layer of property writes on top, read
    /// first by every accessor (the column handle included).
    pub fn with_layer(mut self, tx: &'a PropLayer) -> Self {
        self.layer = Some(tx);
        self
    }

    /// Overlay-only constructor (V5–V7 and the no-base V8 path).
    pub fn owned(overlay: &'a ColumnStore) -> Self {
        Self {
            overlay,
            base: None,
            mixed: None,
            strings: None,
            layer: None,
        }
    }

    /// Construct a view that merges an mmap'd V8 base with a WAL-replay overlay.
    ///
    /// Used when a V8 snapshot is open: `overlay` starts empty (or holds only
    /// post-snapshot mutations) and `base` is the zero-copy archived columns
    /// section from the V8 mmap.
    ///
    /// Prefer [`ColumnsView::with_base_cached`] wherever the owning
    /// [`MappedBase`](crate::v8::MappedBase) is in reach: without a cache every
    /// read of a `Mixed` column decodes that whole column.
    pub fn with_base(overlay: &'a ColumnStore, base: &'a ArchivedColumns) -> Self {
        Self {
            overlay,
            base: Some(base),
            mixed: None,
            strings: None,
            layer: None,
        }
    }

    /// Same as [`ColumnsView::with_base`], but reads of `Mixed` columns are
    /// served from `mixed` instead of decoding the column on every lookup.
    pub fn with_base_cached(
        overlay: &'a ColumnStore,
        base: &'a ArchivedColumns,
        mixed: &'a MixedCache,
    ) -> Self {
        Self {
            overlay,
            base: Some(base),
            mixed: Some(mixed),
            strings: None,
            layer: None,
        }
    }

    /// Attach the snapshot's shared string table (V9 section 12).
    ///
    /// Pass `None` for a pre-V9 base: every `ColumnData::Str` there carries its
    /// own copy of the table and that copy stays authoritative.  Pass the table
    /// from the same [`MappedBase`](crate::v8::MappedBase) the `base` columns
    /// came from — a table from a different snapshot would resolve string ids
    /// against the wrong vocabulary.
    pub fn with_shared_strings(
        mut self,
        strings: Option<&'a crate::v8::layout::ArchivedStringTable>,
    ) -> Self {
        self.strings = strings;
        self
    }

    /// Look up a property value for `(id, field)`: overlay first, then base.
    ///
    /// Returns `ValueRef::Borrowed` for overlay hits (zero allocation) and
    /// `ValueRef::Owned` for base hits (materialises from archived data).
    pub fn get(&self, id: u32, field: &str) -> Option<ValueRef<'_>> {
        // The layer's writes first.
        if let Some(tx) = self.layer {
            if let Some(own) = tx.get(id, field) {
                return own.map(ValueRef::Borrowed);
            }
        }
        // Overlay first.
        if let Some(v) = self.overlay.get(id, field) {
            return Some(ValueRef::Borrowed(v));
        }
        // Prop tombstone check: a RemoveProp for a base-only value records a
        // tombstone so that subsequent reads correctly see the value as absent.
        if self.overlay.is_tombstoned(id, field) {
            return None;
        }
        // Base fallback.
        let base = self.base?;
        let field_entry = base.fields.iter().find(|e| e.name.as_str() == field)?;
        match &field_entry.col {
            ArchivedColumnData::Int { data, present } => {
                if !archived_bitmap_test(present.as_slice(), id) {
                    return None;
                }
                let idx = id as usize;
                if idx >= data.len() {
                    return None;
                }
                Some(ValueRef::Owned(Value::Int(i64::from(data[idx]))))
            }
            ArchivedColumnData::Float { data, present } => {
                if !archived_bitmap_test(present.as_slice(), id) {
                    return None;
                }
                let idx = id as usize;
                if idx >= data.len() {
                    return None;
                }
                Some(ValueRef::Owned(Value::Float(f64::from(data[idx]))))
            }
            ArchivedColumnData::Bool { data, present } => {
                if !archived_bitmap_test(present.as_slice(), id) {
                    return None;
                }
                let idx = id as usize;
                if idx >= data.len() {
                    return None;
                }
                Some(ValueRef::Owned(Value::Bool(data[idx] != 0)))
            }
            ArchivedColumnData::Str {
                ids,
                present,
                strings,
            } => {
                if !archived_bitmap_test(present.as_slice(), id) {
                    return None;
                }
                let idx = id as usize;
                if idx >= ids.len() {
                    return None;
                }
                let sid = u32::from(ids[idx]) as usize;
                // The resolution rule: if the shared table is present it is the
                // table; otherwise the column's own `strings` is.
                let table = match self.strings {
                    Some(shared) => &shared.strings,
                    None => strings,
                };
                if sid >= table.len() {
                    return None;
                }
                Some(ValueRef::Owned(Value::Str(table[sid].as_str().to_string())))
            }
            ArchivedColumnData::Mixed(blob) => match self.mixed {
                // One decode per field for the life of the mapping. The value
                // is cloned out, so the `Arc` is released with this expression.
                Some(cache) => cache
                    .get_or_decode(field, blob.as_slice())
                    .get(&id)
                    .cloned()
                    .map(ValueRef::Owned),
                None => {
                    let map: HashMap<u32, Value> = bincode::deserialize(blob.as_slice()).ok()?;
                    map.get(&id).cloned().map(ValueRef::Owned)
                }
            },
            ArchivedColumnData::Vector { dim, data, present } => {
                if !archived_bitmap_test(present.as_slice(), id) {
                    return None;
                }
                let dim_val = u32::from(*dim) as usize;
                let start = id as usize * dim_val;
                let end = start + dim_val;
                if end > data.len() {
                    return None;
                }
                let floats: Vec<Value> = data[start..end]
                    .iter()
                    .map(|f| Value::Float(f64::from(*f)))
                    .collect();
                Some(ValueRef::Owned(Value::List(floats)))
            }
        }
    }

    /// Every property of node `id`, by field name: overlay first, then the
    /// archived base, with the same precedence and tombstone masking as
    /// [`ColumnsView::get`]. One pass over the columns, allocating only for
    /// the fields the node actually has.
    pub fn props_of(&self, id: u32) -> std::collections::BTreeMap<String, Value> {
        let mut out = std::collections::BTreeMap::new();
        let base_names = self
            .base
            .into_iter()
            .flat_map(|b| b.fields.iter().map(|e| e.name.as_str()));
        let tx_names = self.layer.into_iter().flat_map(PropLayer::fields);
        for field in tx_names.chain(self.overlay.fields()).chain(base_names) {
            if out.contains_key(field) {
                continue;
            }
            if let Some(v) = self.get(id, field) {
                out.insert(field.to_string(), v.into_value());
            }
        }
        out
    }

    /// Return the raw `f64` vector for `(id, field)` from the archived base.
    ///
    /// Returns `None` when:
    ///   - there is no base (V5–V7 or V8 with empty base),
    ///   - the field is not a `Vector` column in the base,
    ///   - node `id` has no value for the field, or
    ///   - the overlay has a value for `(id, field)` (overlay takes priority,
    ///     but that value is accessible via `get()` as a `Value::List`).
    ///
    /// Returns `Cow::Borrowed` when the archived data happens to be 8-byte
    /// aligned (zero-copy fast path), or `Cow::Owned` when it is not
    /// (element-wise copy via `read_unaligned`).
    ///
    /// **Overlay/archive asymmetry**: vectors written after the last snapshot
    /// live in the overlay as `Value::List([Value::Float, ...])`.  Callers
    /// that need `&[f64]` for overlay vectors must call `get()` and convert
    /// manually (`Value::List` → iterate `Value::Float` elements).
    pub fn vector(&self, id: u32, field: &str) -> Option<Cow<'_, [f64]>> {
        // Overlay takes priority: if the overlay has data for (id, field), we
        // do not fall through to base — but we cannot return &[f64] from a
        // Value::List.  See the doc comment above for the asymmetry. A
        // layer is one more overlay here.
        if let Some(tx) = self.layer {
            if tx.get(id, field).is_some() {
                return None;
            }
        }
        if self.overlay.get(id, field).is_some() {
            return None;
        }
        // Prop tombstone check: a RemoveProp for a base-only vector records a
        // tombstone so that subsequent reads correctly see the value as absent.
        if self.overlay.is_tombstoned(id, field) {
            return None;
        }
        let base = self.base?;
        let field_entry = base.fields.iter().find(|e| e.name.as_str() == field)?;
        let (dim, data, present) = match &field_entry.col {
            ArchivedColumnData::Vector { dim, data, present } => (dim, data, present),
            _ => return None,
        };
        if !archived_bitmap_test(present.as_slice(), id) {
            return None;
        }
        let dim_val = u32::from(*dim) as usize;
        if dim_val == 0 {
            return None;
        }
        let start = id as usize * dim_val;
        let end = start + dim_val;
        let archived_slice = data.as_slice();
        if end > archived_slice.len() {
            return None;
        }
        let chunk = &archived_slice[start..end];
        // `rend::F64_le` has align(1), so rkyv does not guarantee 8-byte alignment.
        // Fast path: if the pointer is 8-byte aligned, a zero-copy transmute is sound
        // because the LE compile guard above ensures `Archived<f64>` == `rend::F64_le`
        // has the same wire bytes as `f64` on this target.
        // Slow path: copy each element via `read_unaligned` to avoid UB from a
        // misaligned `&f64` reference — correct and safe on all supported targets.
        let ptr = chunk.as_ptr() as *const f64;
        let result = if ptr.align_offset(std::mem::align_of::<f64>()) == 0 {
            // SAFETY: LE compile guard (line 17-18) ensures same bit representation.
            // Alignment verified above.  Slice length matches chunk.len() elements.
            Cow::Borrowed(unsafe { std::slice::from_raw_parts(ptr, chunk.len()) })
        } else {
            let vec: Vec<f64> = (0..chunk.len())
                .map(|i|
                    // SAFETY: ptr+i is within chunk (bounds checked above).
                    // read_unaligned handles the misaligned access correctly.
                    unsafe { std::ptr::read_unaligned(ptr.add(i)) })
                .collect();
            Cow::Owned(vec)
        };
        Some(result)
    }

    /// Return all field names visible through this view: overlay ∪ base,
    /// deduplicated and sorted.
    ///
    /// Does not filter per-node tombstones — the caller should call `get(id,
    /// field)` for each field, which applies tombstone masking.  Fields that
    /// are tombstoned for a given node will return `None` from `get()`.
    pub fn field_names(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        for f in self.overlay.fields() {
            seen.insert(f.to_string());
        }
        for f in self.layer.into_iter().flat_map(PropLayer::fields) {
            seen.insert(f.to_string());
        }
        if let Some(base) = self.base {
            for e in base.fields.iter() {
                seen.insert(e.name.as_str().to_string());
            }
        }
        seen.into_iter().collect()
    }

    /// Return a pre-resolved column handle for `field`.
    ///
    /// The handle is backed by the overlay only.  Base values are NOT visible
    /// through the returned handle — use `get()` for overlay-then-base reads.
    /// This matches the fused-scan hot path in exec.rs which handles V5–V7 and
    /// V8 post-snapshot overlay data; base reads happen via `get()`.
    ///
    /// The layer, when the view has one, is consulted first, as
    /// [`Self::get`] consults it.
    pub fn column<'f>(&'f self, field: &'f str) -> ViewColumn<'f> {
        ViewColumn {
            layer: self.layer.map(|tx| (tx, field)),
            overlay: self.overlay.column(field),
        }
    }
}

/// Property writes laid over a [`ColumnsView`]: a reader snapshot's
/// unfolded delta tail, and an open transaction's writes (see
/// [`ColumnsView::with_layer`]).
///
/// Sparse by design: a write costs the entry it writes. The store's
/// [`ColumnStore`] keeps dense typed columns, so a write to a high node id
/// there grows the column to that id — the right trade for the store, and a
/// cost proportional to the store's size for a layer writing to one
/// node of a large graph.
#[derive(Debug, Default, Clone)]
pub struct PropLayer {
    /// Per node, per field: `Some(value)` written, `None` removed.
    nodes: HashMap<u32, BTreeMap<String, Option<Value>>>,
    /// Every field this layer has written or removed, for field listings.
    fields: BTreeSet<String>,
}

impl PropLayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Write `field` of `node`.
    pub fn set(&mut self, node: u32, field: &str, value: Value) {
        self.entry(node, field, Some(value));
    }

    /// Remove `field` of `node`, hiding any value below.
    pub fn remove(&mut self, node: u32, field: &str) {
        self.entry(node, field, None);
    }

    /// Forget every write to `node` (a node the layer deleted, whose
    /// properties nothing reads again).
    pub fn remove_all(&mut self, node: u32) {
        self.nodes.remove(&node);
    }

    /// This layer's say on `(node, field)`: `None` when it has none (read
    /// below), `Some(None)` when it removed the field, `Some(Some(v))` when it
    /// wrote `v`.
    #[inline]
    pub fn get(&self, node: u32, field: &str) -> Option<Option<&Value>> {
        self.nodes.get(&node)?.get(field).map(|own| own.as_ref())
    }

    /// Every field this layer has touched, in order.
    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().map(String::as_str)
    }

    fn entry(&mut self, node: u32, field: &str, value: Option<Value>) {
        if !self.fields.contains(field) {
            self.fields.insert(field.to_string());
        }
        self.nodes
            .entry(node)
            .or_default()
            .insert(field.to_string(), value);
    }
}

/// A pre-resolved column of a [`ColumnsView`]: its layer (when
/// there is one) over the overlay's column. See [`ColumnsView::column`].
pub struct ViewColumn<'a> {
    layer: Option<(&'a PropLayer, &'a str)>,
    overlay: ColumnHandle<'a>,
}

impl<'a> ViewColumn<'a> {
    /// The value for `node`: the layer's when it wrote or removed
    /// one, the overlay's otherwise.
    #[inline]
    pub fn get(&self, node: u32) -> Option<&'a Value> {
        if let Some((tx, field)) = self.layer {
            if let Some(own) = tx.get(node, field) {
                return own;
            }
        }
        self.overlay.get(node)
    }
}

// ---------------------------------------------------------------------------
// EdgePropsView
// ---------------------------------------------------------------------------

/// Overlay-over-base edge-property view.
///
/// After a V8 snapshot open the base edge props live in the zero-copy
/// `ArchivedEdgeProps` section.  Only post-snapshot changes are in `overlay`.
/// Tombstones in `overlay` mask base entries for deleted edges.
///
/// This type is `Copy` (two references) and intended to be passed by value.
#[derive(Copy, Clone)]
pub struct EdgePropsView<'a> {
    pub overlay: &'a EdgeProps,
    pub base: Option<&'a ArchivedEdgeProps>,
    /// Edge-property changes of a reader snapshot's unfolded delta tail and
    /// an open transaction, read first: its tombstones hide an edge's
    /// properties below. `None` reads the rest alone. See
    /// [`EdgePropsView::with_layer`].
    pub layer: Option<&'a EdgeProps>,
}

impl<'a> EdgePropsView<'a> {
    /// Overlay-only constructor (no V8 base, or V8 with no prior snapshot).
    pub fn owned(overlay: &'a EdgeProps) -> Self {
        Self {
            overlay,
            base: None,
            layer: None,
        }
    }

    /// Overlay + archived base constructor.
    pub fn with_base(overlay: &'a EdgeProps, base: &'a ArchivedEdgeProps) -> Self {
        Self {
            overlay,
            base: Some(base),
            layer: None,
        }
    }

    /// This view with a layer of edge-property changes on top.
    pub fn with_layer(mut self, tx: &'a EdgeProps) -> Self {
        self.layer = Some(tx);
        self
    }

    /// This view without its layer.
    fn below_tx(&self) -> Self {
        Self {
            overlay: self.overlay,
            base: self.base,
            layer: None,
        }
    }

    /// Look up a field value for `(etype, src, dst)`.
    ///
    /// 1. If tombstoned in overlay → `None` (deletion masking).
    /// 2. If present in overlay map → return overlay value (owned clone).
    /// 3. Binary-search the archived base (zero-copy decode on hit).
    /// 4. `None` if absent everywhere.
    pub fn get(&self, etype: u32, src: u32, dst: u32, field: &str) -> Option<Value> {
        if let Some(tx) = self.layer {
            if tx.is_tombstoned(etype, src, dst) {
                return None;
            }
            if let Some(v) = tx.get(etype, src, dst, field) {
                return Some(v.clone());
            }
            return self.below_tx().get(etype, src, dst, field);
        }
        // Tombstone check — masks both overlay and base entries.
        if self.overlay.is_tombstoned(etype, src, dst) {
            return None;
        }
        // Overlay lookup.
        if let Some(v) = self.overlay.get(etype, src, dst, field) {
            return Some(v.clone());
        }
        // Archive fallback.
        let base = self.base?;
        let entry = base.entries.binary_search_by(|e| {
            let ke = u32::from(e.etype);
            let ks = u32::from(e.src);
            let kd = u32::from(e.dst);
            (ke, ks, kd).cmp(&(etype, src, dst))
        });
        let entry = match entry {
            Ok(i) => &base.entries[i],
            Err(_) => return None,
        };
        let props: std::collections::BTreeMap<String, Value> =
            bincode::deserialize(entry.props_blob.as_slice()).ok()?;
        props.get(field).cloned()
    }

    /// Every property of the edge `(etype, src, dst)`, with the same
    /// precedence as [`EdgePropsView::get`]: a tombstone hides all of it, an
    /// overlay value wins over the archived one, and archived fields the
    /// overlay does not set still show.
    pub fn props_of(
        &self,
        etype: u32,
        src: u32,
        dst: u32,
    ) -> std::collections::BTreeMap<String, Value> {
        if let Some(tx) = self.layer {
            if tx.is_tombstoned(etype, src, dst) {
                return std::collections::BTreeMap::new();
            }
            let mut out = self.below_tx().props_of(etype, src, dst);
            if let Some(own) = tx.props_of(etype, src, dst) {
                out.extend(own.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            return out;
        }
        if self.overlay.is_tombstoned(etype, src, dst) {
            return std::collections::BTreeMap::new();
        }
        let mut out: std::collections::BTreeMap<String, Value> = self
            .base
            .and_then(|base| {
                let i = base
                    .entries
                    .binary_search_by(|e| {
                        let key = (u32::from(e.etype), u32::from(e.src), u32::from(e.dst));
                        key.cmp(&(etype, src, dst))
                    })
                    .ok()?;
                bincode::deserialize(base.entries[i].props_blob.as_slice()).ok()
            })
            .unwrap_or_default();
        if let Some(overlay) = self.overlay.props_of(etype, src, dst) {
            out.extend(overlay.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        out
    }
}

/// Test whether bit `id` is set in an archived bitmap (slice of `Archived<u64>`).
fn archived_bitmap_test(words: &[rkyv::Archived<u64>], id: u32) -> bool {
    let word = id as usize / 64;
    let bit = id as usize % 64;
    if word >= words.len() {
        return false;
    }
    (u64::from(words[word]) >> bit) & 1 == 1
}

#[cfg(test)]
mod value_ref_tests {
    use super::ValueRef;
    use crate::types::Value;

    fn borrowed(v: &Value) -> ValueRef<'_> {
        ValueRef::Borrowed(v)
    }

    fn owned(v: Value) -> ValueRef<'static> {
        ValueRef::Owned(v)
    }

    // All Value variants compared through ValueRef vs owned Value.
    #[test]
    fn value_ref_cross_type_equivalence_battery() {
        use std::collections::BTreeMap;
        let cases: Vec<Value> = vec![
            Value::Int(0),
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
            Value::Float(0.0),
            Value::Float(f64::NAN), // NaN != NaN, so this tests the false branch
            Value::Float(1.5),
            Value::Bool(true),
            Value::Bool(false),
            Value::Str("hello".into()),
            Value::Str("".into()),
            Value::List(vec![Value::Float(1.0), Value::Float(2.0)]),
            Value::List(vec![]),
            Value::Map(BTreeMap::new()),
            Value::Map({
                let mut m = BTreeMap::new();
                m.insert("k".to_string(), Value::Int(1));
                m
            }),
        ];

        for val in &cases {
            // NaN must be handled first: IEEE 754 NaN != NaN for every comparison
            // variant.  All four pairs must assert_ne, then skip the rest of the loop.
            if let Value::Float(f) = val {
                if f.is_nan() {
                    let b = borrowed(val);
                    let o = owned(val.clone());
                    assert_ne!(b, *val, "NaN: Borrowed(v) must not equal v");
                    assert_ne!(o, *val, "NaN: Owned(v) must not equal v");
                    assert_ne!(b, borrowed(val), "NaN: Borrowed == Borrowed must be false");
                    assert_ne!(o, owned(val.clone()), "NaN: Owned == Owned must be false");
                    continue;
                }
            }

            let b = borrowed(val);
            let o = owned(val.clone());

            // Borrowed == owned Value
            assert_eq!(b, *val, "Borrowed(v) == v failed for {val:?}");
            // Owned == owned Value
            assert_eq!(o, *val, "Owned(v) == v failed for {val:?}");
            // Borrowed == Borrowed
            assert_eq!(b, borrowed(val), "Borrowed == Borrowed failed for {val:?}");
            // Owned == Owned
            assert_eq!(o, owned(val.clone()), "Owned == Owned failed for {val:?}");
            // Borrowed == Owned
            assert_eq!(b, o, "Borrowed == Owned failed for {val:?}");
        }
    }

    #[test]
    fn value_ref_cross_type_not_equal() {
        // Different variant types must not compare equal.
        let int_val = Value::Int(1);
        let float_val = Value::Float(1.0);
        assert_ne!(
            borrowed(&int_val),
            borrowed(&float_val),
            "Int(1) must not equal Float(1.0)"
        );
        assert_ne!(
            owned(Value::Bool(true)),
            owned(Value::Int(1)),
            "Bool(true) must not equal Int(1)"
        );
        assert_ne!(
            owned(Value::Map(std::collections::BTreeMap::new())),
            owned(Value::Str("".into())),
            "Map(empty) must not equal Str(empty)"
        );
    }

    #[test]
    fn value_ref_into_value_roundtrip() {
        let val = Value::Str("round-trip".into());
        let b = borrowed(&val);
        assert_eq!(b.into_value(), val);

        let o = owned(Value::Int(42));
        assert_eq!(o.into_value(), Value::Int(42));
    }
}

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
use crate::topology::{Direction, Topology};
use crate::types::Value;
use crate::v8::layout::{ArchivedColumnData, ArchivedColumns, ArchivedCsr};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};

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
}

impl<'a> TopologyView<'a> {
    /// Construct a view backed only by owned overlay (no base — V5/V6/V7 path).
    pub fn owned(overlay: &'a Topology) -> Self {
        Self {
            overlay,
            base: None,
        }
    }

    /// Construct a view that merges an mmap'd V8 base with a WAL-replay overlay.
    ///
    /// Used when a V8 snapshot is open: the base holds the snapshot CSR and the
    /// overlay holds only post-snapshot WAL mutations.
    pub fn with_base(overlay: &'a Topology, base: &'a ArchivedCsr) -> Self {
        Self {
            overlay,
            base: Some(base),
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
        let overlay_nbrs = self.overlay.neighbors(etype, dir, v);
        let base = match self.base {
            None => return overlay_nbrs,
            Some(b) => b,
        };

        // Locate the etype entry in the sorted archived CSR.
        let base_nbrs = base_neighbors_from_archived(base, etype, dir, v);

        if base_nbrs.is_empty() {
            return overlay_nbrs;
        }

        // Subtract overlay tombstones from base neighbors before merging.
        // When the overlay records an edge deletion for an edge that exists only
        // in the base CSR, `remove_edge` writes a tombstone into the overlay.
        // That tombstone must be filtered out here so deleted edges do not
        // reappear after a V8 snapshot open.
        let filtered_base_nbrs = subtract_tombstones(base_nbrs, etype, dir, v, self.overlay);

        if filtered_base_nbrs.is_empty() {
            return overlay_nbrs;
        }
        if overlay_nbrs.is_empty() {
            return Cow::Owned(filtered_base_nbrs);
        }
        Cow::Owned(merge_sorted_unique(
            overlay_nbrs.as_ref(),
            &filtered_base_nbrs,
        ))
    }

    /// Total edge count: (base - tombstones) + overlay.
    ///
    /// Tombstones in the overlay represent edges that were present only in the
    /// base CSR and were subsequently deleted via `remove_edge`.  They must be
    /// subtracted from the base count to avoid phantom edge counts.
    pub fn edge_count(&self) -> u64 {
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

    /// Edge-type ids present in overlay and/or base, sorted ascending.
    pub fn etypes(&self) -> std::vec::IntoIter<u32> {
        match self.base {
            None => {
                // Fast path: collect from overlay (already sorted by Topology::etypes).
                self.overlay.etypes().collect::<Vec<_>>().into_iter()
            }
            Some(base) => {
                let mut set: BTreeSet<u32> = self.overlay.etypes().collect();
                for et in base.etypes.iter() {
                    set.insert(u32::from(et.etype));
                }
                set.into_iter().collect::<Vec<_>>().into_iter()
            }
        }
    }
}

/// Look up neighbors in an archived CSR for `(etype, dir, v)`.
/// Returns an empty Vec if not found.
fn base_neighbors_from_archived(
    base: &ArchivedCsr,
    etype: u32,
    dir: Direction,
    v: u32,
) -> Vec<u32> {
    // Binary search for the etype (etypes are sorted by etype ascending).
    let et_pos = base
        .etypes
        .binary_search_by_key(&etype, |e| u32::from(e.etype));
    let et_entry = match et_pos {
        Ok(i) => &base.etypes[i],
        Err(_) => return Vec::new(),
    };

    let adj = match dir {
        Direction::Out => &et_entry.out_adj,
        Direction::In => &et_entry.in_adj,
    };

    // Binary search for the vertex (rows are sorted by vertex ascending).
    let row_pos = adj.rows.binary_search_by_key(&v, |r| u32::from(r.vertex));
    let row = match row_pos {
        Ok(i) => &adj.rows[i],
        Err(_) => return Vec::new(),
    };

    // Collect neighbor ids (stored as archived u32 LE values).
    row.neighbors.iter().map(|n| u32::from(*n)).collect()
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
    use crate::topology::Topology;
    use crate::v8::encode::{encode_v8, V8Meta};
    use crate::v8::MappedBase;
    use std::collections::BTreeMap;

    fn tiny_meta() -> V8Meta {
        V8Meta {
            labels: vec![],
            edge_props: crate::edge_props::EdgeProps::new(),
            rule_defs: vec![],
            provenance: BTreeMap::new(),
            rule_tripped: BTreeMap::new(),
            rule_fires: BTreeMap::new(),
            ivf_state: BTreeMap::new(),
            view_defs: vec![],
            wal_truncated: false,
            hnsw: BTreeMap::new(),
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
        let was_in_overlay = overlay.remove_edge(etype, a, b);
        assert!(!was_in_overlay, "edge was base-only, not in overlay");

        // Build the merged view.
        let view = TopologyView {
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

/// Overlay-over-base column store view.
///
/// Reads consult the owned overlay `ColumnStore` first; if the field/node is
/// absent in the overlay, the archived base section is checked.  Writes always
/// go through the WAL into the owned overlay (never through this view).
///
/// **Column handle**: `ColumnsView::column()` returns an overlay-backed
/// `ColumnHandle`.  Base values are NOT visible through the column handle —
/// only through the `get()` path.  This is acceptable because:
///   (a) base is `None` for V5–V7 stores and for V8 stores before Task 3
///       wires the persistent mmap (overlay starts empty after snapshot open);
///   (b) callers that need a fused-scan path with base values should use
///       `get()` directly.
#[derive(Copy, Clone)]
pub struct ColumnsView<'a> {
    pub overlay: &'a ColumnStore,
    pub base: Option<&'a crate::v8::layout::ArchivedColumns>,
}

impl<'a> ColumnsView<'a> {
    /// Overlay-only constructor (V5–V7 and the no-base V8 path).
    pub fn owned(overlay: &'a ColumnStore) -> Self {
        Self {
            overlay,
            base: None,
        }
    }

    /// Construct a view that merges an mmap'd V8 base with a WAL-replay overlay.
    ///
    /// Used when a V8 snapshot is open: `overlay` starts empty (or holds only
    /// post-snapshot mutations) and `base` is the zero-copy archived columns
    /// section from the V8 mmap.
    pub fn with_base(overlay: &'a ColumnStore, base: &'a ArchivedColumns) -> Self {
        Self {
            overlay,
            base: Some(base),
        }
    }

    /// Look up a property value for `(id, field)`: overlay first, then base.
    ///
    /// Returns `ValueRef::Borrowed` for overlay hits (zero allocation) and
    /// `ValueRef::Owned` for base hits (materialises from archived data).
    pub fn get(&self, id: u32, field: &str) -> Option<ValueRef<'_>> {
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
                if sid >= strings.len() {
                    return None;
                }
                Some(ValueRef::Owned(Value::Str(
                    strings[sid].as_str().to_string(),
                )))
            }
            ArchivedColumnData::Mixed(blob) => {
                let map: HashMap<u32, Value> = bincode::deserialize(blob.as_slice()).ok()?;
                map.get(&id).cloned().map(ValueRef::Owned)
            }
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

    /// Return the raw `f64` vector for `(id, field)` from the archived base.
    ///
    /// Returns `None` when:
    ///   - there is no base (V5–V7 or V8 with empty base),
    ///   - the field is not a `Vector` column in the base,
    ///   - node `id` has no value for the field, or
    ///   - the overlay has a value for `(id, field)` (overlay takes priority,
    ///     but that value is accessible via `get()` as a `Value::List`).
    ///
    /// **Overlay/archive asymmetry**: vectors written after the last snapshot
    /// live in the overlay as `Value::List([Value::Float, ...])`.  Callers
    /// that need `&[f64]` for overlay vectors must call `get()` and convert
    /// manually (`Value::List` → iterate `Value::Float` elements).
    ///
    /// # Safety of the returned slice
    ///
    /// On little-endian targets (the only supported platform, per the LE-pinned
    /// format constraint) `rkyv::Archived<f64>` has the same bit representation
    /// as `f64`.  The transmute in the implementation is sound under this
    /// constraint.
    pub fn vector(&self, id: u32, field: &str) -> Option<&[f64]> {
        // Overlay takes priority: if the overlay has data for (id, field), we
        // do not fall through to base — but we cannot return &[f64] from a
        // Value::List.  See the doc comment above for the asymmetry.
        if self.overlay.get(id, field).is_some() {
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
        // SAFETY: On little-endian targets (our only supported platform, enforced
        // by the LE-pinned V8 format constraint), `rkyv::Archived<f64>` (= the
        // `rend::F64_le` type) has the same 8-byte IEEE-754 representation as
        // `f64`.  The archived slice is guaranteed to be 8-byte aligned by rkyv's
        // serializer.  Transmuting `&[Archived<f64>]` to `&[f64]` is therefore a
        // no-op on LE hardware and produces a valid, properly aligned slice.
        let f64_slice: &[f64] =
            unsafe { std::slice::from_raw_parts(chunk.as_ptr() as *const f64, chunk.len()) };
        Some(f64_slice)
    }

    /// Return a pre-resolved column handle for `field`.
    ///
    /// The handle is backed by the overlay only.  Base values are NOT visible
    /// through the returned handle — use `get()` for overlay-then-base reads.
    /// This matches the fused-scan hot path in exec.rs which handles V5–V7 and
    /// V8 post-snapshot overlay data; base reads happen via `get()`.
    pub fn column(&self, field: &str) -> ColumnHandle<'_> {
        self.overlay.column(field)
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

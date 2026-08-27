//! View seams: overlay-over-base read paths for topology and columns.
//!
//! `TopologyView` is wired into `GraphView.topo` so all topology reads
//! transparently consult overlay then base.  `ColumnsView` and `ValueRef`
//! are defined here and available for callers that need merged column reads;
//! `GraphView.props` is wired in Task 2 once the `ValueRef` API surface is
//! finalised (see report DONE_WITH_CONCERNS).

use crate::columns::ColumnStore;
use crate::topology::{Direction, Topology};
use crate::types::Value;
use crate::v8::layout::ArchivedCsr;
use std::borrow::Cow;
use std::collections::BTreeSet;

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
        if overlay_nbrs.is_empty() {
            return Cow::Owned(base_nbrs);
        }

        // Merge two sorted-unique lists.
        Cow::Owned(merge_sorted_unique(overlay_nbrs.as_ref(), &base_nbrs))
    }

    /// Total edge count: overlay + base (no double-counting since both are
    /// disjoint at Task-1 open time).
    pub fn edge_count(&self) -> u64 {
        let ov = self.overlay.edge_count();
        let bv = self
            .base
            .map(|b| {
                // SAFETY: edge_count is a u64 archived in LE via rend; converting
                // to native u64 is always correct.
                u64::from(b.edge_count)
            })
            .unwrap_or(0);
        ov + bv
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

/// An iterator over the sorted-unique merged neighbor list.
/// Produced by `TopologyView::neighbors`; callers use `.as_ref()`.
///
/// This type alias is exported to satisfy the Task-1 interface contract.
pub type MergedNeighbors<'a> = Cow<'a, [u32]>;

// ---------------------------------------------------------------------------
// ColumnsView and ValueRef (defined; wired in Task 2)
// ---------------------------------------------------------------------------

/// A borrowed or materialized column value.
///
/// For the Task-1 owned path, only `Borrowed` is produced.  Task 2 adds
/// `Owned` for values materialised from the archived column sections.
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
/// Task 1: `base` is always `None`; column reads go to overlay only.
/// Task 2 wires `base` and routes reads through `get()` → `ValueRef`.
///
/// `GraphView.props` remains `&'a ColumnStore` for Task 1 (see report).
pub struct ColumnsView<'a> {
    pub overlay: &'a ColumnStore,
    pub base: Option<&'a crate::v8::layout::ArchivedColumns>,
}

impl<'a> ColumnsView<'a> {
    /// Overlay-only constructor (V5–V7 and V8 Task-1 path).
    pub fn owned(overlay: &'a ColumnStore) -> Self {
        Self {
            overlay,
            base: None,
        }
    }

    /// Look up a property value for `(id, field)`: overlay first, then base.
    ///
    /// For Task 1 with `base = None`, this is identical to
    /// `overlay.get(id, field)` wrapped in `ValueRef::Borrowed`.
    pub fn get(&self, id: u32, field: &str) -> Option<ValueRef<'_>> {
        if let Some(v) = self.overlay.get(id, field) {
            return Some(ValueRef::Borrowed(v));
        }
        // Task 2: materialise from self.base when present.
        let _base = self.base?;
        None
    }

    /// Return the raw `f64` vector for `(id, field)` if the column is a
    /// list of floats (Task 2; always `None` in Task 1).
    pub fn vector(&self, id: u32, field: &str) -> Option<&[f64]> {
        let _ = (id, field);
        None
    }
}

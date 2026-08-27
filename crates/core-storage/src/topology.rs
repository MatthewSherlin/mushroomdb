use crate::pack::{push_u32, push_u32s, push_u64, read_u32, read_u32s, read_u64};
use crate::types::Result as StoreResult;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Out,
    In,
}

/// Flush the per-vertex insert buffer into the frozen block once it exceeds this.
const INSERT_BUFFER: usize = 32;

/// Sorted frozen neighbors plus an unsorted insert buffer. Serializes as `Vec<u32>`
/// (merged, sorted, unique) so V6 snapshots stay HashMap-of-HashMap-of-Vec.
#[derive(Debug, Default, Clone)]
pub(crate) struct AdjList {
    pub(crate) frozen: Vec<u32>,
    pub(crate) delta: Vec<u32>,
}

impl AdjList {
    fn contains(&self, id: u32) -> bool {
        self.frozen.binary_search(&id).is_ok() || self.delta.contains(&id)
    }

    fn push(&mut self, id: u32) {
        self.delta.push(id);
        if self.delta.len() > INSERT_BUFFER {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.delta.is_empty() {
            return;
        }
        if self.frozen.is_empty() {
            self.delta.sort_unstable();
            self.delta.dedup();
            std::mem::swap(&mut self.frozen, &mut self.delta);
            return;
        }
        self.frozen = merge_sorted_unique(&self.frozen, &self.delta);
        self.delta.clear();
    }

    pub(crate) fn merged(&self) -> Vec<u32> {
        merge_sorted_unique(&self.frozen, &self.delta)
    }

    fn remove(&mut self, id: u32) -> bool {
        if let Ok(pos) = self.frozen.binary_search(&id) {
            self.frozen.remove(pos);
            return true;
        }
        if let Some(pos) = self.delta.iter().position(|&x| x == id) {
            self.delta.swap_remove(pos);
            return true;
        }
        false
    }
}

impl Serialize for AdjList {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        if self.delta.is_empty() {
            self.frozen.serialize(serializer)
        } else {
            self.merged().serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for AdjList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Ok(Self {
            frozen: Vec::<u32>::deserialize(deserializer)?,
            delta: Vec::new(),
        })
    }
}

fn merge_sorted_unique(frozen: &[u32], delta: &[u32]) -> Vec<u32> {
    let mut extra: Vec<u32> = delta.to_vec();
    extra.sort_unstable();
    extra.dedup();
    if frozen.is_empty() {
        return extra;
    }
    if extra.is_empty() {
        return frozen.to_vec();
    }
    let mut out = Vec::with_capacity(frozen.len() + extra.len());
    let mut i = 0;
    let mut j = 0;
    while i < frozen.len() && j < extra.len() {
        match frozen[i].cmp(&extra[j]) {
            Ordering::Less => {
                out.push(frozen[i]);
                i += 1;
            }
            Ordering::Greater => {
                out.push(extra[j]);
                j += 1;
            }
            Ordering::Equal => {
                out.push(frozen[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&frozen[i..]);
    out.extend_from_slice(&extra[j..]);
    out
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct TypedAdjacency {
    pub(crate) out: HashMap<u32, AdjList>,
    pub(crate) inn: HashMap<u32, AdjList>,
}

/// Typed adjacency: per `(etype, dir, vertex)` a frozen sorted block plus an
/// unsorted insert buffer. `neighbors` borrows the frozen block when the buffer
/// is empty; otherwise it returns a sorted-unique merge.
///
/// On-disk (V6) shape is still `HashMap<u32, {out, inn: HashMap<u32, Vec<u32>>}>`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Topology {
    pub(crate) by_type: HashMap<u32, TypedAdjacency>,
    edge_count: u64,
    /// Out-direction tombstones: edges deleted from the base CSR that are absent
    /// from the overlay.  Keyed by `(etype → src → {deleted dst ids})`.
    ///
    /// Populated by `remove_edge` when the edge is not found in the overlay —
    /// this records a deletion of a base-only edge so `TopologyView::neighbors`
    /// can subtract it from the base CSR when merging.
    ///
    /// Not serialised: tombstones are eliminated at snapshot-merge time
    /// (encode_v8 subtracts them when building the merged CSR section).
    #[serde(skip)]
    pub(crate) out_tombstones: HashMap<u32, HashMap<u32, BTreeSet<u32>>>,
    /// In-direction tombstones: symmetric index of `out_tombstones`.
    /// Keyed by `(etype → dst → {deleted src ids})` for O(log n) In lookups.
    #[serde(skip)]
    pub(crate) in_tombstones: HashMap<u32, HashMap<u32, BTreeSet<u32>>>,
}

impl Topology {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_edge(&mut self, etype: u32, src: u32, dst: u32) -> bool {
        let adj = self.by_type.entry(etype).or_default();
        let dsts = adj.out.entry(src).or_default();
        if dsts.contains(dst) {
            return false;
        }
        dsts.push(dst);
        let srcs = adj.inn.entry(dst).or_default();
        assert!(
            !srcs.contains(src),
            "invariant: inn must not contain src as neighbor of dst when out lacks dst"
        );
        srcs.push(src);
        self.edge_count += 1;
        true
    }

    pub fn neighbors(&self, etype: u32, dir: Direction, v: u32) -> Cow<'_, [u32]> {
        match self.adj_list(etype, dir, v) {
            None => Cow::Borrowed(&[]),
            Some(n) if n.delta.is_empty() => Cow::Borrowed(&n.frozen),
            Some(n) => Cow::Owned(n.merged()),
        }
    }

    pub fn degree(&self, etype: u32, dir: Direction, v: u32) -> usize {
        self.neighbors(etype, dir, v).as_ref().len()
    }

    pub fn edge_count(&self) -> u64 {
        self.edge_count
    }

    /// Edge-type ids present in the graph, sorted ascending.
    /// `by_type` stays a `HashMap` (smaller than a BTreeMap migrate; snapshot encoding unchanged);
    /// this method collect+sorts keys so iteration is deterministic.
    pub fn etypes(&self) -> impl Iterator<Item = u32> + '_ {
        let mut ids: Vec<u32> = self.by_type.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
    }

    /// Iterate all directed out-edges as `(etype, src, dst)` triples.
    ///
    /// Yields every edge in the overlay topology in arbitrary order.  Used by
    /// callers (e.g. `core-api`) that need to walk the full edge set without
    /// direct access to the private `by_type` field.
    pub fn all_edges(&self) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
        self.by_type.iter().flat_map(|(&etype, adj)| {
            adj.out.iter().flat_map(move |(&src, al)| {
                al.merged().into_iter().map(move |dst| (etype, src, dst))
            })
        })
    }

    pub fn remove_edge(&mut self, etype: u32, src: u32, dst: u32) -> bool {
        let found_in_overlay = (|| {
            let adj = self.by_type.get_mut(&etype)?;
            let dsts = adj.out.get_mut(&src)?;
            if !dsts.remove(dst) {
                return None;
            }
            let srcs = adj
                .inn
                .get_mut(&dst)
                .expect("invariant: inn bucket must exist when out contains dst");
            assert!(
                srcs.remove(src),
                "invariant: inn must contain src when out contained dst"
            );
            self.edge_count -= 1;
            Some(())
        })()
        .is_some();

        if !found_in_overlay {
            // The edge was not in the overlay (may be present only in the mmap'd base
            // CSR).  Record a tombstone so `TopologyView::neighbors` can subtract it
            // from the base when merging overlay + base for reads and for snapshot-merge.
            self.out_tombstones
                .entry(etype)
                .or_default()
                .entry(src)
                .or_default()
                .insert(dst);
            self.in_tombstones
                .entry(etype)
                .or_default()
                .entry(dst)
                .or_default()
                .insert(src);
        }

        found_in_overlay
    }

    /// Return the set of `dst` ids tombstoned for `(etype, Direction::Out, src)`.
    ///
    /// Used by `TopologyView::neighbors` to subtract deleted edges from the base CSR.
    pub fn out_tombstones_for(&self, etype: u32, src: u32) -> Option<&BTreeSet<u32>> {
        self.out_tombstones.get(&etype)?.get(&src)
    }

    /// Return the set of `src` ids tombstoned for `(etype, Direction::In, dst)`.
    pub fn in_tombstones_for(&self, etype: u32, dst: u32) -> Option<&BTreeSet<u32>> {
        self.in_tombstones.get(&etype)?.get(&dst)
    }

    fn adj_list(&self, etype: u32, dir: Direction, v: u32) -> Option<&AdjList> {
        self.by_type.get(&etype).and_then(|adj| match dir {
            Direction::Out => adj.out.get(&v),
            Direction::In => adj.inn.get(&v),
        })
    }

    /// V7 packed CSR: etype count, then per etype (id, out map, in map), then edge_count.
    /// Each adjacency map is vertex-count + (vertex, length-prefixed frozen neighbor array).
    /// Deltas are merged into frozen on pack; unpack leaves deltas empty.
    pub(crate) fn pack(&self, out: &mut Vec<u8>) {
        let mut etypes: Vec<u32> = self.by_type.keys().copied().collect();
        etypes.sort_unstable();
        push_u32(out, etypes.len() as u32);
        for et in etypes {
            push_u32(out, et);
            let adj = &self.by_type[&et];
            pack_adj_map(out, &adj.out);
            pack_adj_map(out, &adj.inn);
        }
        push_u64(out, self.edge_count);
    }

    pub(crate) fn unpack(src: &[u8]) -> StoreResult<(Self, usize)> {
        let mut pos = 0usize;
        let n_etypes = read_u32(src, &mut pos)? as usize;
        let mut by_type = HashMap::with_capacity(n_etypes);
        for _ in 0..n_etypes {
            let et = read_u32(src, &mut pos)?;
            let out = unpack_adj_map(src, &mut pos)?;
            let inn = unpack_adj_map(src, &mut pos)?;
            by_type.insert(et, TypedAdjacency { out, inn });
        }
        let edge_count = read_u64(src, &mut pos)?;
        Ok((
            Self {
                by_type,
                edge_count,
                out_tombstones: HashMap::new(),
                in_tombstones: HashMap::new(),
            },
            pos,
        ))
    }
}

fn pack_adj_map(out: &mut Vec<u8>, map: &HashMap<u32, AdjList>) {
    let mut verts: Vec<u32> = map.keys().copied().collect();
    verts.sort_unstable();
    push_u32(out, verts.len() as u32);
    for v in verts {
        push_u32(out, v);
        let list = &map[&v];
        if list.delta.is_empty() {
            push_u32s(out, &list.frozen);
        } else {
            let merged = list.merged();
            push_u32s(out, &merged);
        }
    }
}

fn unpack_adj_map(src: &[u8], pos: &mut usize) -> StoreResult<HashMap<u32, AdjList>> {
    let n = read_u32(src, pos)? as usize;
    let mut map = HashMap::with_capacity(n);
    for _ in 0..n {
        let v = read_u32(src, pos)?;
        let frozen = read_u32s(src, pos)?;
        map.insert(
            v,
            AdjList {
                frozen,
                delta: Vec::new(),
            },
        );
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_are_typed_directed_sorted_deduped() {
        let mut t = Topology::new();
        assert!(t.add_edge(0, 5, 9));
        assert!(t.add_edge(0, 5, 3));
        assert!(!t.add_edge(0, 5, 9)); // duplicate
        assert!(t.add_edge(1, 5, 9)); // same pair, different type: distinct edge
        assert_eq!(t.neighbors(0, Direction::Out, 5).as_ref(), &[3, 9]); // sorted
        assert_eq!(t.neighbors(0, Direction::In, 9).as_ref(), &[5]);
        assert_eq!(t.neighbors(0, Direction::Out, 999).as_ref(), &[] as &[u32]);
        assert_eq!(t.degree(0, Direction::Out, 5), 2);
        assert_eq!(t.edge_count(), 3);
    }

    #[test]
    fn remove_edge_updates_both_sides_and_count() {
        let mut t = Topology::new();
        t.add_edge(0, 1, 2);
        t.add_edge(0, 1, 3);
        assert!(t.remove_edge(0, 1, 2));
        assert!(!t.remove_edge(0, 1, 2)); // idempotent-false
        assert!(!t.remove_edge(9, 1, 2)); // unknown type
        assert_eq!(t.neighbors(0, Direction::Out, 1).as_ref(), &[3]);
        assert_eq!(t.neighbors(0, Direction::In, 2).as_ref(), &[] as &[u32]);
        assert_eq!(t.edge_count(), 1);
        // re-add after remove works
        assert!(t.add_edge(0, 1, 2));
        assert_eq!(t.edge_count(), 2);
    }

    #[test]
    fn etypes_empty_multiple_and_sorted() {
        let empty = Topology::new();
        assert_eq!(empty.etypes().collect::<Vec<_>>(), Vec::<u32>::new());

        let mut t = Topology::new();
        t.add_edge(3, 0, 1);
        t.add_edge(1, 0, 1);
        t.add_edge(3, 1, 2); // existing type, must not duplicate
        t.add_edge(2, 0, 2);
        assert_eq!(t.etypes().collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn insert_buffer_defers_sort_until_threshold_then_neighbors_sorted_unique() {
        let mut t = Topology::new();
        // Reverse inserts: a full sort on every add would keep the scan path borrowed.
        for dst in (0u32..32).rev() {
            assert!(t.add_edge(0, 0, dst));
            let nbrs = t.neighbors(0, Direction::Out, 0);
            assert!(
                matches!(nbrs, Cow::Owned(_)),
                "delta still dirty at {} edges; neighbors must take the owned merge path",
                32 - dst
            );
            assert!(
                nbrs.windows(2).all(|w| w[0] < w[1]),
                "dirty merge must be sorted unique, got {nbrs:?}"
            );
            assert_eq!(nbrs.len(), (32 - dst) as usize);
            let n = t.adj_list(0, Direction::Out, 0).unwrap();
            assert!(
                n.frozen.is_empty(),
                "must not flush frozen before threshold"
            );
            assert_eq!(n.delta.len(), (32 - dst) as usize);
        }
        assert_eq!(t.degree(0, Direction::Out, 0), 32);
        assert_eq!(t.neighbors(0, Direction::In, 31).as_ref(), &[0]);

        assert!(t.add_edge(0, 0, 32)); // buffer len > 32 → merge into frozen
        let nbrs = t.neighbors(0, Direction::Out, 0);
        assert!(
            matches!(nbrs, Cow::Borrowed(_)),
            "after threshold flush, scan path must borrow the frozen block"
        );
        let expected: Vec<u32> = (0..33).collect();
        assert_eq!(nbrs.as_ref(), expected.as_slice());
        let n = t.adj_list(0, Direction::Out, 0).unwrap();
        assert!(n.delta.is_empty());
        assert_eq!(n.frozen, expected);
        assert_eq!(t.edge_count(), 33);
        assert_eq!(t.neighbors(0, Direction::In, 32).as_ref(), &[0]);
        assert!(!t.add_edge(0, 0, 7));
    }

    #[test]
    fn remove_edge_from_delta_and_from_frozen() {
        let mut t = Topology::new();
        for dst in 0u32..10 {
            assert!(t.add_edge(0, 1, dst));
        }
        assert!(matches!(t.neighbors(0, Direction::Out, 1), Cow::Owned(_)));
        assert!(t.remove_edge(0, 1, 7));
        assert!(!t.remove_edge(0, 1, 7));
        assert_eq!(t.neighbors(0, Direction::In, 7).as_ref(), &[] as &[u32]);
        assert_eq!(
            t.neighbors(0, Direction::Out, 1).as_ref(),
            &[0, 1, 2, 3, 4, 5, 6, 8, 9]
        );
        assert_eq!(t.edge_count(), 9);
        assert!(t.add_edge(0, 1, 7));
        assert_eq!(t.edge_count(), 10);

        let mut t = Topology::new();
        for dst in 0u32..33 {
            assert!(t.add_edge(0, 1, dst));
        }
        assert!(matches!(
            t.neighbors(0, Direction::Out, 1),
            Cow::Borrowed(_)
        ));
        assert!(t.remove_edge(0, 1, 0));
        assert!(t.remove_edge(0, 1, 32));
        assert_eq!(t.edge_count(), 31);
        assert_eq!(t.neighbors(0, Direction::In, 0).as_ref(), &[] as &[u32]);
        assert_eq!(t.neighbors(0, Direction::In, 16).as_ref(), &[1]);
        let expected: Vec<u32> = (1..32).collect();
        assert_eq!(
            t.neighbors(0, Direction::Out, 1).as_ref(),
            expected.as_slice()
        );
        assert!(t.add_edge(0, 1, 0));
        assert_eq!(t.edge_count(), 32);
    }

    #[test]
    fn serde_wire_is_hashmap_of_hashmap_of_vec() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct WireAdj {
            out: HashMap<u32, Vec<u32>>,
            inn: HashMap<u32, Vec<u32>>,
        }
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wire {
            by_type: HashMap<u32, WireAdj>,
            edge_count: u64,
        }

        let mut by_type = HashMap::new();
        by_type.insert(
            0,
            WireAdj {
                out: HashMap::from([(5, vec![3, 9])]),
                inn: HashMap::from([(3, vec![5]), (9, vec![5])]),
            },
        );
        let wire = Wire {
            by_type,
            edge_count: 2,
        };
        let encoded = bincode::serialize(&wire).unwrap();
        let t: Topology = bincode::deserialize(&encoded).unwrap();
        assert_eq!(t.neighbors(0, Direction::Out, 5).as_ref(), &[3, 9]);
        assert_eq!(t.neighbors(0, Direction::In, 3).as_ref(), &[5]);
        assert_eq!(t.neighbors(0, Direction::In, 9).as_ref(), &[5]);
        assert_eq!(t.edge_count(), 2);
        assert!(matches!(
            t.neighbors(0, Direction::Out, 5),
            Cow::Borrowed(_)
        ));

        let roundtrip: Wire = bincode::deserialize(&bincode::serialize(&t).unwrap()).unwrap();
        assert_eq!(roundtrip.edge_count, 2);
        assert_eq!(roundtrip.by_type[&0].out[&5], vec![3, 9]);
        assert_eq!(roundtrip.by_type[&0].inn[&3], vec![5]);
        assert_eq!(roundtrip.by_type[&0].inn[&9], vec![5]);

        // Dirty delta still encodes as a sorted unique Vec.
        let mut dirty = Topology::new();
        for dst in (0u32..10).rev() {
            dirty.add_edge(1, 0, dst);
        }
        assert!(matches!(
            dirty.neighbors(1, Direction::Out, 0),
            Cow::Owned(_)
        ));
        let dirty_wire: Wire = bincode::deserialize(&bincode::serialize(&dirty).unwrap()).unwrap();
        assert_eq!(dirty_wire.edge_count, 10);
        assert_eq!(
            dirty_wire.by_type[&1].out[&0],
            (0..10).collect::<Vec<u32>>()
        );
        for dst in 0..10 {
            assert_eq!(dirty_wire.by_type[&1].inn[&dst], vec![0]);
        }
    }

    #[test]
    fn pack_roundtrip_merges_delta_and_restores_frozen() {
        let mut t = Topology::new();
        for dst in (0u32..10).rev() {
            t.add_edge(2, 1, dst);
        }
        t.add_edge(0, 5, 9);
        assert!(matches!(t.neighbors(2, Direction::Out, 1), Cow::Owned(_)));
        let mut buf = Vec::new();
        t.pack(&mut buf);
        let (back, consumed) = Topology::unpack(&buf).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(back.edge_count(), 11);
        let expected: Vec<u32> = (0..10).collect();
        assert_eq!(
            back.neighbors(2, Direction::Out, 1).as_ref(),
            expected.as_slice()
        );
        assert!(matches!(
            back.neighbors(2, Direction::Out, 1),
            Cow::Borrowed(_)
        ));
        assert_eq!(back.neighbors(0, Direction::Out, 5).as_ref(), &[9]);
        assert_eq!(back.neighbors(0, Direction::In, 9).as_ref(), &[5]);
        assert_eq!(back.etypes().collect::<Vec<_>>(), vec![0, 2]);
    }
}

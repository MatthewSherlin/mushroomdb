use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;

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
struct AdjList {
    frozen: Vec<u32>,
    delta: Vec<u32>,
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

    fn merged(&self) -> Vec<u32> {
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
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.delta.is_empty() {
            self.frozen.serialize(serializer)
        } else {
            self.merged().serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for AdjList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
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
struct TypedAdjacency {
    out: HashMap<u32, AdjList>,
    inn: HashMap<u32, AdjList>,
}

/// Typed adjacency: per `(etype, dir, vertex)` a frozen sorted block plus an
/// unsorted insert buffer. `neighbors` borrows the frozen block when the buffer
/// is empty; otherwise it returns a sorted-unique merge.
///
/// On-disk (V6) shape is still `HashMap<u32, {out, inn: HashMap<u32, Vec<u32>>}>`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Topology {
    by_type: HashMap<u32, TypedAdjacency>,
    edge_count: u64,
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

    pub fn remove_edge(&mut self, etype: u32, src: u32, dst: u32) -> bool {
        let Some(adj) = self.by_type.get_mut(&etype) else {
            return false;
        };
        let Some(dsts) = adj.out.get_mut(&src) else {
            return false;
        };
        if !dsts.remove(dst) {
            return false;
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
        true
    }

    fn adj_list(&self, etype: u32, dir: Direction, v: u32) -> Option<&AdjList> {
        self.by_type.get(&etype).and_then(|adj| match dir {
            Direction::Out => adj.out.get(&v),
            Direction::In => adj.inn.get(&v),
        })
    }
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
}

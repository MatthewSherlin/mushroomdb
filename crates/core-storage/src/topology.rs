use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Out,
    In,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct TypedAdjacency {
    out: HashMap<u32, Vec<u32>>, // src -> sorted dsts
    inn: HashMap<u32, Vec<u32>>, // dst -> sorted srcs
}

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
        match dsts.binary_search(&dst) {
            Ok(_) => false,
            Err(pos) => {
                dsts.insert(pos, dst);
                let srcs = adj.inn.entry(dst).or_default();
                let p = srcs.binary_search(&src).expect_err(
                    "invariant: inn must not contain src as neighbor of dst when out lacks dst",
                );
                srcs.insert(p, src);
                self.edge_count += 1;
                true
            }
        }
    }

    pub fn neighbors(&self, etype: u32, dir: Direction, v: u32) -> &[u32] {
        self.by_type
            .get(&etype)
            .and_then(|adj| match dir {
                Direction::Out => adj.out.get(&v),
                Direction::In => adj.inn.get(&v),
            })
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn degree(&self, etype: u32, dir: Direction, v: u32) -> usize {
        self.neighbors(etype, dir, v).len()
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
        let Ok(pos) = dsts.binary_search(&dst) else {
            return false;
        };
        dsts.remove(pos);
        let srcs = adj
            .inn
            .get_mut(&dst)
            .expect("invariant: inn bucket must exist when out contains dst");
        let p = srcs
            .binary_search(&src)
            .expect("invariant: inn must contain src when out contained dst");
        srcs.remove(p);
        self.edge_count -= 1;
        true
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
        assert_eq!(t.neighbors(0, Direction::Out, 5), &[3, 9]); // sorted
        assert_eq!(t.neighbors(0, Direction::In, 9), &[5]);
        assert_eq!(t.neighbors(0, Direction::Out, 999), &[] as &[u32]);
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
        assert_eq!(t.neighbors(0, Direction::Out, 1), &[3]);
        assert_eq!(t.neighbors(0, Direction::In, 2), &[] as &[u32]);
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
}

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
}

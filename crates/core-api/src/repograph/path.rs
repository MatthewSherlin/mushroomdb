//! The walk between two nodes that no single rule explains.
//!
//! When nothing links two files directly, the useful answer is not "no" but
//! "through here": the shortest chain of imports, calls, co-changes and
//! mentions that reaches one from the other. Breadth-first, so the first path
//! found is a shortest one, and bounded by hops so a question about two
//! unrelated corners of a repository costs no more than a question about two
//! neighbours.

use crate::db::GraphDb;
use crate::repograph::facts::neighbors;
use crate::Direction;
use core_storage::fs::Fs;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The edge types a path may be walked over, in the order the digests name
/// them. Every one of them says one part of a repository depends on, changes
/// with, or talks about another.
pub const PATH_EDGES: [&str; 4] = ["IMPORTS", "CALLS", "CO_CHANGED", "MENTIONS"];

/// Longest chain a `why` fallback will look for.
pub const MAX_HOPS: usize = 6;

/// The shortest chain of `edge_types` edges from `a` to `b`, as
/// `(edge type, node reached)` hops — so a two-hop answer is
/// `[(IMPORTS, x), (CO_CHANGED, b)]` and `a` is the caller's own starting
/// point.
///
/// Edges are followed in both directions: `a` importing `x` and `x` importing
/// `a` both say the two are connected, and a reader asking how two files relate
/// does not care which way the arrow points. Empty when `a` and `b` are the
/// same node, when either is unknown, or when no chain of at most `max_hops`
/// reaches one from the other.
///
/// Deterministic: neighbours are visited in `(key, edge type)` order, so of
/// several shortest paths the same one always comes back.
#[must_use]
pub fn shortest_path<F: Fs>(
    db: &GraphDb<F>,
    a: &str,
    b: &str,
    edge_types: &[&str],
    max_hops: usize,
) -> Vec<(String, String)> {
    if a == b || max_hops == 0 || !db.has_node(a) || !db.has_node(b) {
        return Vec::new();
    }
    // How each node was first reached: the edge walked and the node it came
    // from. Filled in discovery order, which the sort below pins.
    let mut came_from: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut seen: BTreeSet<String> = [a.to_string()].into_iter().collect();
    let mut queue: VecDeque<(String, usize)> = [(a.to_string(), 0)].into_iter().collect();

    while let Some((node, depth)) = queue.pop_front() {
        if depth == max_hops {
            continue;
        }
        for (next, etype) in step(db, &node, edge_types) {
            if !seen.insert(next.clone()) {
                continue;
            }
            came_from.insert(next.clone(), (etype, node.clone()));
            if next == b {
                return unwind(&came_from, a, b);
            }
            queue.push_back((next, depth + 1));
        }
    }
    Vec::new()
}

/// Every node one edge away from `node`, as `(neighbour, edge type)` sorted by
/// neighbour and then edge type. A neighbour reachable over two edge types
/// appears once, under the type that sorts first — the path prints one edge per
/// hop, and this is which one it names.
fn step<F: Fs>(db: &GraphDb<F>, node: &str, edge_types: &[&str]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for etype in edge_types {
        for dir in [Direction::Out, Direction::In] {
            for nbr in neighbors(db, node, etype, dir) {
                out.push((nbr, (*etype).to_string()));
            }
        }
    }
    out.sort();
    out.dedup_by(|x, y| x.0 == y.0);
    out
}

/// Walk the `came_from` chain back from `b` and hand it out forwards.
fn unwind(
    came_from: &BTreeMap<String, (String, String)>,
    a: &str,
    b: &str,
) -> Vec<(String, String)> {
    let mut hops: Vec<(String, String)> = Vec::new();
    let mut node = b.to_string();
    while node != a {
        let Some((etype, prev)) = came_from.get(&node) else {
            return Vec::new(); // unreachable: every seen node but `a` has one
        };
        hops.push((etype.clone(), node.clone()));
        node = prev.clone();
    }
    hops.reverse();
    hops
}

//! In-tree HNSW approximate nearest-neighbor index.
//!
//! Implements the Malkov & Yashunin (2018) Hierarchical Navigable Small World
//! algorithm with:
//!   - M = 32 (max connections per layer above layer 0)
//!   - M₀ = 64 (max connections at layer 0)
//!   - ef_construction = 400 (beam width during insertion)
//!   - ef_search = 400 (beam width during query)
//!
//! Parameters are set for high-dimensional text embeddings (768-d to 2048-d).
//! At these dimensionalities the nearest-neighbour distribution is flat; larger
//! M and ef are required to route through the hierarchy and recover true k-NN.
//!
//! All vectors are L2-normalized at insert time; cosine similarity reduces to
//! dot product for unit vectors, which is faster and numerically stable.
//!
//! **Determinism**: the level assigned to each node is derived from a seeded
//! PRNG (`splitmix64`) seeded with `FNV-1a(rule_name) XOR (node_id × PHI)`.
//! Insertion order + seed fully determines the graph structure, so WAL replay
//! produces an identical index.

use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

/// Maximum layer cap: prevents pathological depth on tiny graphs.
const MAX_LEVEL: usize = 16;
/// `M` — max connections per layer (except layer 0).
pub const M: usize = 32;
/// `M₀` — max connections at layer 0.
pub const M0: usize = 64;
/// Beam width for insertion.
pub const EF_CONSTRUCTION: usize = 400;
/// Beam width for search.
pub const EF_SEARCH: usize = 400;

// ---------------------------------------------------------------------------
// PRNG helpers
// ---------------------------------------------------------------------------

/// splitmix64 step — one round of the splitmix64 PRNG.
#[inline]
fn splitmix64(x: u64) -> u64 {
    let x = x.wrapping_add(0x9E3779B97F4A7C15);
    let x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    let x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

/// Generate the insertion level for `node_id` using the rule's `base_seed`.
///
/// Level follows the geometric distribution used by HNSW:
///   `l = floor(-ln(uniform) / ln(M))`
/// where `uniform` is deterministically derived from the seed.
fn gen_level(base_seed: u64, node_id: u32) -> usize {
    // Fibonacci-hash the node id to spread seeds uniformly.
    let mixed = base_seed ^ (node_id as u64).wrapping_mul(0x9E3779B97F4A7C15);
    let rng = splitmix64(mixed);
    // Map upper 53 bits to (0, 1] — avoids ln(0).
    let bits = (rng >> 11) | 1; // ensure non-zero
    let uniform = bits as f64 / (1u64 << 53) as f64;
    let ml = 1.0 / (M as f64).ln();
    let level = (-uniform.ln() * ml).floor() as usize;
    level.min(MAX_LEVEL)
}

// ---------------------------------------------------------------------------
// f64 ordering wrapper (for BinaryHeap)
// ---------------------------------------------------------------------------

/// f64 wrapper implementing total order (NaN sorts last).
#[derive(Debug, Clone, Copy, PartialEq)]
struct OrdF64(f64);

impl Eq for OrdF64 {}
impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(std::cmp::Ordering::Greater)
    }
}

// ---------------------------------------------------------------------------
// Node storage
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
struct HnswNode {
    /// Assigned layer level (inclusive; node has layers 0..=level).
    level: usize,
    /// L2-normalized unit vector.
    vector: Vec<f64>,
    /// `layers[l]` = neighbor node ids at layer `l`.
    layers: Vec<Vec<u32>>,
}

// ---------------------------------------------------------------------------
// HnswIndex
// ---------------------------------------------------------------------------

/// In-tree HNSW approximate nearest-neighbor index for cosine similarity.
///
/// Stores L2-normalized vectors and answers approximate k-NN queries using the
/// Malkov & Yashunin hierarchical graph.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HnswIndex {
    /// Base seed derived from `fnv1a(rule_name)`. Mixed with each node's id
    /// to generate deterministic per-node levels.
    base_seed: u64,
    nodes: BTreeMap<u32, HnswNode>,
    entry_point: Option<u32>,
    max_level: usize,
}

impl HnswIndex {
    /// Create a new empty HNSW index seeded by `base_seed`.
    ///
    /// Typically `base_seed = fnv1a(rule_name.as_bytes())` so WAL replay
    /// with the same rule name always produces the same graph structure.
    pub fn new(base_seed: u64) -> Self {
        Self {
            base_seed,
            nodes: BTreeMap::new(),
            entry_point: None,
            max_level: 0,
        }
    }

    /// Number of indexed vectors.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True when no vectors are indexed.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Returns all node ids currently in the index.
    pub fn node_ids(&self) -> BTreeSet<u32> {
        self.nodes.keys().copied().collect()
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Cosine distance from node `id` to unit query `q`.
    /// `id` must exist in `nodes`. Returns `1 - dot(v_id, q)` clamped to
    /// [0, 2] (distance 0 = identical, 2 = opposite).
    #[inline]
    fn dist_to(nodes: &BTreeMap<u32, HnswNode>, id: u32, q: &[f64]) -> f64 {
        let v = &nodes[&id].vector;
        let dot: f64 = v.iter().zip(q.iter()).map(|(a, b)| a * b).sum();
        (1.0 - dot.clamp(-1.0, 1.0)).max(0.0)
    }

    /// Beam search on a single layer.
    ///
    /// Returns a list of `(node_id, cosine_distance)` — the `ef` nearest
    /// candidates found starting from `ep`. Ascending distance order is not
    /// guaranteed (callers sort as needed).
    fn beam_search(
        nodes: &BTreeMap<u32, HnswNode>,
        q: &[f64],
        ep: u32,
        layer: usize,
        ef: usize,
    ) -> Vec<(u32, f64)> {
        // visited: avoid re-expanding a node
        let mut visited = std::collections::BTreeSet::new();
        visited.insert(ep);

        let ep_dist = Self::dist_to(nodes, ep, q);

        // c_heap: min-heap of (dist, id) — candidates to expand
        let mut c_heap: BinaryHeap<Reverse<(OrdF64, u32)>> = BinaryHeap::new();
        c_heap.push(Reverse((OrdF64(ep_dist), ep)));

        // w_heap: max-heap of (dist, id) — ef-best results (worst on top for eviction)
        let mut w_heap: BinaryHeap<(OrdF64, u32)> = BinaryHeap::new();
        w_heap.push((OrdF64(ep_dist), ep));

        while let Some(&Reverse((OrdF64(c_dist), c))) = c_heap.peek() {
            // furthest in result set
            let f_dist = w_heap.peek().map(|(OrdF64(d), _)| *d).unwrap_or(f64::MAX);
            if c_dist > f_dist {
                break; // all remaining candidates are farther than our worst result
            }
            c_heap.pop();

            let neighbors = nodes
                .get(&c)
                .and_then(|n| n.layers.get(layer))
                .cloned()
                .unwrap_or_default();

            for e in neighbors {
                if visited.contains(&e) {
                    continue;
                }
                if !nodes.contains_key(&e) {
                    continue; // defensive: stale neighbor ref after remove
                }
                visited.insert(e);

                let e_dist = Self::dist_to(nodes, e, q);
                let f_dist = w_heap.peek().map(|(OrdF64(d), _)| *d).unwrap_or(f64::MAX);
                if e_dist < f_dist || w_heap.len() < ef {
                    c_heap.push(Reverse((OrdF64(e_dist), e)));
                    w_heap.push((OrdF64(e_dist), e));
                    if w_heap.len() > ef {
                        w_heap.pop(); // evict furthest
                    }
                }
            }
        }

        w_heap
            .into_iter()
            .map(|(OrdF64(d), id)| (id, d))
            .collect()
    }

    /// Greedy 1-NN descent from `ep` at `layer`. Returns the nearest node
    /// found (used for upper-layer descent during insert/search).
    fn greedy_step(nodes: &BTreeMap<u32, HnswNode>, q: &[f64], ep: u32, layer: usize) -> u32 {
        let mut curr = ep;
        let mut curr_dist = Self::dist_to(nodes, ep, q);
        loop {
            let neighbors = nodes
                .get(&curr)
                .and_then(|n| n.layers.get(layer))
                .cloned()
                .unwrap_or_default();
            let mut improved = false;
            for nb in neighbors {
                if !nodes.contains_key(&nb) {
                    continue;
                }
                let d = Self::dist_to(nodes, nb, q);
                if d < curr_dist {
                    curr_dist = d;
                    curr = nb;
                    improved = true;
                }
            }
            if !improved {
                break;
            }
        }
        curr
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Insert vector `v` for node `id`.
    ///
    /// Zero vectors are silently skipped (cosine is undefined for them).
    /// If `id` already exists it is replaced (remove + re-insert semantics).
    pub fn insert(&mut self, id: u32, v: &[f64]) {
        let Some(unit) = l2_normalize(v) else {
            return; // zero vector — skip
        };

        // Remove existing entry if any (handles update = remove + re-insert).
        if self.nodes.contains_key(&id) {
            self.remove(id);
        }

        let level = gen_level(self.base_seed, id);

        if self.entry_point.is_none() {
            // First node ever inserted.
            self.nodes.insert(
                id,
                HnswNode {
                    level,
                    vector: unit,
                    layers: vec![vec![]; level + 1],
                },
            );
            self.entry_point = Some(id);
            self.max_level = level;
            return;
        }

        let ep = self.entry_point.unwrap();
        let max_level = self.max_level;
        let mut curr_ep = ep;

        // Phase 1: greedy descent from max_level to level+1 (ef=1).
        for lc in ((level + 1)..=max_level).rev() {
            curr_ep = Self::greedy_step(&self.nodes, &unit, curr_ep, lc);
        }

        // Phase 2: beam-search + connect at each layer from min(level, max_level)
        // down to 0.
        let mut per_layer_neighbors: Vec<Vec<u32>> = vec![vec![]; level + 1];
        for lc in (0..=level.min(max_level)).rev() {
            let m_lc = if lc == 0 { M0 } else { M };

            // Beam search to collect ef_construction nearest candidates.
            let mut candidates =
                Self::beam_search(&self.nodes, &unit, curr_ep, lc, EF_CONSTRUCTION);

            // Sort ascending by distance and take M nearest as neighbors.
            candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let neighbors: Vec<u32> = candidates.iter().take(m_lc).map(|(id, _)| *id).collect();
            per_layer_neighbors[lc] = neighbors.clone();

            // Advance curr_ep to the nearest candidate.
            if let Some(&(nearest, _)) = candidates.first() {
                curr_ep = nearest;
            }

            // Add bidirectional links and prune over-connected neighbors.
            for &nb_id in &neighbors {
                if let Some(nb_node) = self.nodes.get_mut(&nb_id) {
                    while nb_node.layers.len() <= lc {
                        nb_node.layers.push(vec![]);
                    }
                    if !nb_node.layers[lc].contains(&id) {
                        nb_node.layers[lc].push(id);
                    }
                }

                // Prune if over-connected (simple selection: keep M nearest to nb_id).
                let over_limit = self
                    .nodes
                    .get(&nb_id)
                    .and_then(|n| n.layers.get(lc))
                    .map(|l| l.len() > m_lc)
                    .unwrap_or(false);

                if over_limit {
                    let nb_vec: Vec<f64> = self.nodes[&nb_id].vector.clone();
                    let current: Vec<u32> = self.nodes[&nb_id].layers[lc].clone();

                    // Score each current neighbor by distance to nb_id.
                    let mut scored: Vec<(u32, f64)> = current
                        .iter()
                        .map(|&nid| {
                            let d = if nid == id {
                                let dot: f64 = nb_vec.iter().zip(unit.iter()).map(|(a, b)| a * b).sum();
                                (1.0 - dot.clamp(-1.0, 1.0)).max(0.0)
                            } else if let Some(n) = self.nodes.get(&nid) {
                                let dot: f64 = nb_vec.iter().zip(n.vector.iter()).map(|(a, b)| a * b).sum();
                                (1.0 - dot.clamp(-1.0, 1.0)).max(0.0)
                            } else {
                                f64::MAX
                            };
                            (nid, d)
                        })
                        .collect();

                    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                    scored.truncate(m_lc);
                    self.nodes.get_mut(&nb_id).unwrap().layers[lc] =
                        scored.into_iter().map(|(nid, _)| nid).collect();
                }
            }
        }

        // Insert the new node.
        self.nodes.insert(
            id,
            HnswNode {
                level,
                vector: unit,
                layers: per_layer_neighbors,
            },
        );

        // Update entry point if new node has a higher level.
        if level > max_level {
            self.entry_point = Some(id);
            self.max_level = level;
        }
    }

    /// Remove node `id` from the index.
    ///
    /// All back-references from other nodes are cleaned up. If `id` was the
    /// entry point, a new entry point is elected (highest remaining level).
    pub fn remove(&mut self, id: u32) {
        let Some(node) = self.nodes.remove(&id) else {
            return;
        };

        // Remove id from all of its declared neighbors' lists.
        for (lc, neighbors) in node.layers.iter().enumerate() {
            for &nb_id in neighbors {
                if let Some(nb_node) = self.nodes.get_mut(&nb_id) {
                    if lc < nb_node.layers.len() {
                        nb_node.layers[lc].retain(|&x| x != id);
                    }
                }
            }
        }

        // Defensively scan all remaining nodes for any back-references to `id`
        // (asymmetric pruning can leave links not recorded in `node.layers`).
        for other_node in self.nodes.values_mut() {
            for layer in other_node.layers.iter_mut() {
                layer.retain(|&x| x != id);
            }
        }

        // Update entry point if needed.
        if self.entry_point == Some(id) {
            if self.nodes.is_empty() {
                self.entry_point = None;
                self.max_level = 0;
            } else {
                let (new_ep, new_level) = self
                    .nodes
                    .iter()
                    .max_by_key(|(_, n)| n.level)
                    .map(|(&nid, n)| (nid, n.level))
                    .unwrap();
                self.entry_point = Some(new_ep);
                self.max_level = new_level;
            }
        }
    }

    /// Approximate k-nearest-neighbor search by cosine similarity.
    ///
    /// Returns up to `k` results as `(node_id, cosine_similarity)` pairs,
    /// sorted descending by similarity. Zero-norm query vectors return empty.
    pub fn search(&self, q: &[f64], k: usize) -> Vec<(u32, f64)> {
        let Some(unit_q) = l2_normalize(q) else {
            return vec![];
        };
        let Some(ep) = self.entry_point else {
            return vec![];
        };
        if k == 0 {
            return vec![];
        }

        let ef = k.max(EF_SEARCH);
        let mut curr_ep = ep;

        // Greedy descent from max_level to layer 1.
        for lc in (1..=self.max_level).rev() {
            curr_ep = Self::greedy_step(&self.nodes, &unit_q, curr_ep, lc);
        }

        // Beam search at layer 0 with ef candidates.
        let candidates = Self::beam_search(&self.nodes, &unit_q, curr_ep, 0, ef);

        // Convert distances → cosine similarities; sort descending; take k.
        let mut results: Vec<(u32, f64)> = candidates
            .into_iter()
            .map(|(id, dist)| (id, (1.0 - dist).clamp(-1.0, 1.0)))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(k);
        results
    }
}

// ---------------------------------------------------------------------------
// Vector helpers
// ---------------------------------------------------------------------------

/// L2-normalize `v`. Returns `None` for the zero vector.
pub(crate) fn l2_normalize(v: &[f64]) -> Option<Vec<f64>> {
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm == 0.0 {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `n` deterministic unit vectors in `dim` dimensions using a
    /// splitmix64 PRNG. Each vector is L2-normalized.
    fn make_unit_vecs(n: usize, dim: usize, seed: u64) -> Vec<Vec<f64>> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                let raw: Vec<f64> = (0..dim)
                    .map(|_| {
                        state = splitmix64(state);
                        // Map to [-1, 1]
                        (state as i64 as f64) / (i64::MAX as f64)
                    })
                    .collect();
                l2_normalize(&raw).unwrap_or_else(|| vec![1.0; dim])
            })
            .collect()
    }

    /// Exact brute-force k-NN by cosine (dot product for unit vecs).
    fn exact_knn(vecs: &[Vec<f64>], q: &[f64], k: usize) -> Vec<usize> {
        let unit_q = l2_normalize(q).unwrap();
        let mut scores: Vec<(usize, f64)> = vecs
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let dot: f64 = v.iter().zip(unit_q.iter()).map(|(a, b)| a * b).sum();
                (i, dot)
            })
            .collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.into_iter().take(k).map(|(i, _)| i).collect()
    }

    #[test]
    fn hnsw_recalls_near_duplicate() {
        // 200 deterministic unit vectors in dim 32.
        let vecs = make_unit_vecs(200, 32, 0xDEAD_BEEF_1234_5678);
        let seed = crate::index::fnv1a_u64(b"test-rule");
        let mut idx = HnswIndex::new(seed);
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }

        // Query: vec[7] + tiny noise (so nearest is definitely 7).
        let mut noisy = vecs[7].clone();
        noisy[0] += 1e-4;
        noisy[1] -= 1e-4;

        let results = idx.search(&noisy, 1);
        assert!(!results.is_empty(), "HNSW must return at least one result");
        assert_eq!(
            results[0].0, 7,
            "nearest to vec[7]+noise must be 7, got {} (cos={:.6})",
            results[0].0, results[0].1
        );
    }

    #[test]
    fn hnsw_empty_returns_empty() {
        let idx = HnswIndex::new(42);
        assert!(idx.search(&[1.0, 0.0], 5).is_empty());
    }

    #[test]
    fn hnsw_zero_vector_skipped() {
        let seed = 1;
        let mut idx = HnswIndex::new(seed);
        idx.insert(0, &[0.0, 0.0]); // zero vector — skipped
        idx.insert(1, &[1.0, 0.0]);
        // Only node 1 was actually inserted.
        assert_eq!(idx.len(), 1);
        let r = idx.search(&[1.0, 0.0], 5);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 1);
    }

    #[test]
    fn hnsw_remove_works() {
        let seed = crate::index::fnv1a_u64(b"rm-test");
        let mut idx = HnswIndex::new(seed);
        idx.insert(0, &[1.0, 0.0]);
        idx.insert(1, &[0.0, 1.0]);
        idx.insert(2, &[1.0, 0.0]); // same direction as 0
        idx.remove(0);
        // Search for [1,0] — 0 is gone, 2 is the nearest remaining.
        let r = idx.search(&[1.0, 0.0], 1);
        assert!(!r.is_empty());
        assert_eq!(r[0].0, 2, "after removing 0, nearest must be 2");
    }

    #[test]
    fn hnsw_cosine_order_preserved() {
        let seed = 99;
        let mut idx = HnswIndex::new(seed);
        // node 0: [1,0] (cos=1.0 with query)
        // node 1: [0.6, 0.8] (cos=0.6 with [1,0] query)
        // node 2: [0,1] (cos=0.0 with [1,0] query)
        idx.insert(0, &[1.0, 0.0]);
        idx.insert(1, &[0.6, 0.8]);
        idx.insert(2, &[0.0, 1.0]);
        let r = idx.search(&[1.0, 0.0], 3);
        assert_eq!(r.len(), 3);
        // Results must be descending by cosine.
        assert!(r[0].1 >= r[1].1);
        assert!(r[1].1 >= r[2].1);
        assert_eq!(r[0].0, 0, "node 0 must be nearest");
    }

    /// Recall probe: 5 000 vectors × dim 1536, 50 queries, min recall@10 ≥ 0.90.
    ///
    /// Run with: `cargo test --release -p mushroomdb-rules -- hnsw_5k_1536_recall --ignored`
    ///
    /// Fixed seed — never random per run. Asserts are gates for the CI report.
    #[test]
    #[ignore]
    fn hnsw_5k_1536_recall() {
        const N: usize = 5_000;
        const DIM: usize = 1_536;
        const N_QUERIES: usize = 50;
        const K: usize = 10;

        let seed = crate::index::fnv1a_u64(b"recall-probe-5k-1536");
        let vecs = make_unit_vecs(N, DIM, seed);

        let mut idx = HnswIndex::new(seed);
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }

        // Use a different seed for query vectors so they differ from index vecs.
        let q_seed = crate::index::fnv1a_u64(b"recall-queries");
        let queries = make_unit_vecs(N_QUERIES, DIM, q_seed);

        let mut recalls = Vec::with_capacity(N_QUERIES);
        for q in &queries {
            let exact_set: std::collections::BTreeSet<usize> =
                exact_knn(&vecs, q, K).into_iter().collect();
            let approx_ids: Vec<usize> = idx.search(q, K).into_iter().map(|(id, _)| id as usize).collect();
            let hits = approx_ids.iter().filter(|id| exact_set.contains(id)).count();
            recalls.push(hits as f64 / K as f64);
        }

        let min_recall = recalls.iter().cloned().fold(f64::MAX, f64::min);
        let mean_recall = recalls.iter().sum::<f64>() / recalls.len() as f64;

        eprintln!("HNSW 5k/1536 recall@{K}: min={min_recall:.4} mean={mean_recall:.4}");

        assert!(
            min_recall >= 0.90,
            "min recall@{K} = {min_recall:.4} < 0.90"
        );
        assert!(
            mean_recall >= 0.95,
            "mean recall@{K} = {mean_recall:.4} < 0.95"
        );
    }
}

//! In-tree HNSW approximate nearest-neighbor index.
//!
//! Implements the Malkov & Yashunin (2018) Hierarchical Navigable Small World
//! algorithm with:
//!   - M = 32 (max connections per layer above layer 0)
//!   - M₀ = 128 (max connections at layer 0)
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
///
/// Raised from 64 → 128 (v0.4.2) to support clustered-density graphs where
/// natural clusters exceed 64 nodes. At M₀=64, nodes beyond the 64th in a
/// cluster become inbound-only leaves invisible to beam search, capping recall
/// at ~64/cluster_size. M₀=128 raises the density ceiling to ~128-node
/// clusters; it does NOT remove the structural pathology — the HNSW §3.5
/// diverse-neighbour heuristic is the true fix, ledgered for v0.4.3+.
/// Memory cost: layer-0 adjacency roughly doubles (~256 B → ~512 B/node;
/// ~256 MB at the 500k-node ceiling). Disclosed in CHANGELOG.
pub const M0: usize = 128;
/// Beam width for insertion.
pub const EF_CONSTRUCTION: usize = 400;
/// Beam width for search.
pub const EF_SEARCH: usize = 400;

// ---------------------------------------------------------------------------
// Test hooks
// ---------------------------------------------------------------------------
//
// A thread-local count of vectors pushed into any `HnswIndex` on this thread,
// in the shape `with_ivf_drift_rebuild` (`index.rs:29-50`) already uses. It
// exists so an integration test can assert that opening a store inserts
// *nothing* — the persisted graph is adopted, not rebuilt. Gated on
// `test-hooks` because `#[cfg(test)]` items in this crate are invisible to
// `mushroomdb`'s integration tests.

#[cfg(any(test, feature = "test-hooks"))]
thread_local! {
    static HNSW_INSERT_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static HNSW_REMOVE_SCANNED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count one `HnswIndex::insert` call. Compiles away without `test-hooks`.
#[inline]
fn note_insert() {
    #[cfg(any(test, feature = "test-hooks"))]
    HNSW_INSERT_COUNT.with(|c| c.set(c.get().saturating_add(1)));
}

/// Count `n` nodes whose adjacency a `remove` had to touch. This is the
/// operation count the O(in-degree) claim is made about — no wall clock.
#[inline]
fn note_remove_scanned(n: usize) {
    #[cfg(any(test, feature = "test-hooks"))]
    HNSW_REMOVE_SCANNED.with(|c| c.set(c.get().saturating_add(n as u64)));
    #[cfg(not(any(test, feature = "test-hooks")))]
    let _ = n;
}

/// Nodes whose adjacency lists `HnswIndex::remove` has touched on this thread
/// since the last reset.
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_remove_scanned() -> u64 {
    HNSW_REMOVE_SCANNED.with(|c| c.get())
}

/// Reset this thread's removal-scan counter to zero.
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_remove_scanned_reset() {
    HNSW_REMOVE_SCANNED.with(|c| c.set(0));
}

/// Vectors inserted into any `HnswIndex` on this thread since the last reset.
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_insert_count() -> u64 {
    HNSW_INSERT_COUNT.with(|c| c.get())
}

/// Reset this thread's `HnswIndex::insert` counter to zero.
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_insert_count_reset() {
    HNSW_INSERT_COUNT.with(|c| c.set(0));
}

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

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct HnswNode {
    /// Assigned layer level (inclusive; node has layers 0..=level).
    level: usize,
    /// L2-normalized unit vector.
    vector: Vec<f64>,
    /// `layers[l]` = neighbor **slots** at layer `l`.
    ///
    /// Slots, not node ids: a distance is then a `Vec` index rather than a
    /// `BTreeMap<u32, HnswNode>` descent, and a beam step is a pointer offset.
    /// Adjacency therefore only ever names live slots — a stale slot would name
    /// whichever node reused it, so `remove` must strip every reference to a
    /// slot before freeing it. `back_refs` is what makes that affordable.
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
    /// Dense node storage, indexed by slot. Freed slots are kept (blanked) and
    /// reused, so a slot index is stable for as long as the node lives.
    slots: Vec<HnswNode>,
    /// node id → slot. The authority on which node ids are in the index.
    slot_of: BTreeMap<u32, u32>,
    /// slot → node id. `u32::MAX` marks a freed slot; that doubles as the
    /// liveness check the search paths need.
    id_of: Vec<u32>,
    /// Freed slots, reused by the next insert (LIFO).
    free: Vec<u32>,
    /// Reverse adjacency: slot → the slots that list it as a neighbour on *any*
    /// layer. Maintained in lockstep with `HnswNode::layers` by insert, remove
    /// and the prune. Turns removal from O(live nodes × M₀) into O(in-degree).
    ///
    /// Derivable from `slots`, so it is never serialized: a load rebuilds it
    /// from the decoded adjacency lists (see `rebuild_back_refs`).
    #[serde(skip)]
    back_refs: BTreeMap<u32, BTreeSet<u32>>,
    /// Entry-point **slot**.
    entry_point: Option<u32>,
    max_level: usize,
}

/// A freed slot's id sentinel.
const DEAD: u32 = u32::MAX;

impl HnswIndex {
    /// Create a new empty HNSW index seeded by `base_seed`.
    ///
    /// Typically `base_seed = fnv1a(rule_name.as_bytes())` so WAL replay
    /// with the same rule name always produces the same graph structure.
    pub fn new(base_seed: u64) -> Self {
        Self {
            base_seed,
            ..Self::default()
        }
    }

    /// Number of indexed vectors.
    pub fn len(&self) -> usize {
        self.slot_of.len()
    }

    /// True when no vectors are indexed.
    pub fn is_empty(&self) -> bool {
        self.slot_of.is_empty()
    }

    /// Returns all node ids currently in the index.
    pub fn node_ids(&self) -> BTreeSet<u32> {
        self.slot_of.keys().copied().collect()
    }

    // -----------------------------------------------------------------------
    // Slot bookkeeping
    // -----------------------------------------------------------------------

    /// True when `slot` names a live node.
    #[inline]
    fn is_live(id_of: &[u32], slot: u32) -> bool {
        id_of.get(slot as usize).is_some_and(|&i| i != DEAD)
    }

    /// Bind `id` to a slot (reusing a freed one when available) and store
    /// `node` there. The caller owns linking it into the graph.
    fn alloc_slot(&mut self, id: u32, node: HnswNode) -> u32 {
        let slot = match self.free.pop() {
            Some(s) => {
                self.slots[s as usize] = node;
                self.id_of[s as usize] = id;
                s
            }
            None => {
                self.slots.push(node);
                self.id_of.push(id);
                (self.slots.len() - 1) as u32
            }
        };
        self.slot_of.insert(id, slot);
        slot
    }

    // -----------------------------------------------------------------------
    // Reverse adjacency
    // -----------------------------------------------------------------------

    /// Replace `slot`'s layer-`lc` adjacency with `next`, keeping `back_refs`
    /// in lockstep. The single write path for an adjacency list, so the
    /// invariant `back_refs[t] == {s : t ∈ slots[s].layers[*]}` holds by
    /// construction.
    fn set_layer(&mut self, slot: u32, lc: usize, next: Vec<u32>) {
        let next_set: BTreeSet<u32> = next.iter().copied().collect();
        let prev = std::mem::replace(&mut self.slots[slot as usize].layers[lc], next);
        let prev_set: BTreeSet<u32> = prev.into_iter().collect();

        for &old in prev_set.difference(&next_set) {
            // Still listed on another layer? Then the back-ref stands.
            if self.slots[slot as usize]
                .layers
                .iter()
                .any(|l| l.contains(&old))
            {
                continue;
            }
            if let Some(refs) = self.back_refs.get_mut(&old) {
                refs.remove(&slot);
                if refs.is_empty() {
                    self.back_refs.remove(&old);
                }
            }
        }
        for &added in next_set.difference(&prev_set) {
            self.back_refs.entry(added).or_default().insert(slot);
        }
    }

    /// Record that `from` now lists `target` as a neighbour.
    fn link_back_ref(&mut self, target: u32, from: u32) {
        self.back_refs.entry(target).or_default().insert(from);
    }

    /// The slots that list `slot` as a neighbour on any layer.
    fn referrers_of(&self, slot: u32) -> Vec<u32> {
        self.back_refs
            .get(&slot)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Forget who lists `slot`; the caller has just stripped them all.
    fn clear_back_refs(&mut self, slot: u32) {
        self.back_refs.remove(&slot);
    }

    /// Up-convert a decoded 0.6.5 index. Slots are assigned in ascending node-id
    /// order — the order a fresh build over ascending ids would have produced —
    /// so the result is deterministic, and adjacency ids are remapped onto
    /// them. Ids naming a node the map does not hold are dropped, which is what
    /// the old `nodes.contains_key` guard in the search paths did anyway.
    fn from_v1(v1: HnswIndexV1) -> Self {
        let id_of: Vec<u32> = v1.nodes.keys().copied().collect();
        let slot_of: BTreeMap<u32, u32> = id_of
            .iter()
            .enumerate()
            .map(|(s, &id)| (id, s as u32))
            .collect();
        let slots: Vec<HnswNode> = v1
            .nodes
            .into_values()
            .map(|n| HnswNode {
                level: n.level,
                vector: n.vector,
                layers: n
                    .layers
                    .into_iter()
                    .map(|l| l.iter().filter_map(|id| slot_of.get(id).copied()).collect())
                    .collect(),
            })
            .collect();

        let mut out = Self {
            base_seed: v1.base_seed,
            entry_point: v1.entry_point.and_then(|e| slot_of.get(&e).copied()),
            max_level: v1.max_level,
            slots,
            slot_of,
            id_of,
            free: Vec::new(),
            back_refs: BTreeMap::new(),
        };
        // A dangling entry point would have panicked the old `dist_to`; elect a
        // live one instead.
        if out.entry_point.is_none() && !out.slot_of.is_empty() {
            let (_, &ep) = out
                .slot_of
                .iter()
                .max_by_key(|(_, &s)| out.slots[s as usize].level)
                .expect("slot_of is non-empty");
            out.entry_point = Some(ep);
            out.max_level = out.slots[ep as usize].level;
        }
        out.rebuild_back_refs();
        out
    }

    /// Derive `back_refs` from the adjacency lists. Used after a load, where
    /// the field is not serialized. No distance is computed.
    fn rebuild_back_refs(&mut self) {
        let mut refs: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for (s, node) in self.slots.iter().enumerate() {
            if !Self::is_live(&self.id_of, s as u32) {
                continue;
            }
            for layer in &node.layers {
                for &t in layer {
                    refs.entry(t).or_default().insert(s as u32);
                }
            }
        }
        self.back_refs = refs;
    }

    /// Release `slot`. The caller must already have stripped every adjacency
    /// reference to it — a reused slot names a different node.
    fn free_slot(&mut self, slot: u32) {
        let id = std::mem::replace(&mut self.id_of[slot as usize], DEAD);
        self.slot_of.remove(&id);
        self.slots[slot as usize] = HnswNode::default();
        self.free.push(slot);
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Cosine distance from the node in `slot` to unit query `q`.
    /// Returns `1 - dot(v_slot, q)` clamped to [0, 2] (0 = identical,
    /// 2 = opposite).
    #[inline]
    fn dist_to(slots: &[HnswNode], slot: u32, q: &[f64]) -> f64 {
        let v = &slots[slot as usize].vector;
        let dot: f64 = v.iter().zip(q.iter()).map(|(a, b)| a * b).sum();
        (1.0 - dot.clamp(-1.0, 1.0)).max(0.0)
    }

    /// Beam search on a single layer.
    ///
    /// Returns a list of `(slot, cosine_distance)` — the `ef` nearest
    /// candidates found starting from `ep`. Ascending distance order is not
    /// guaranteed (callers sort as needed).
    fn beam_search(
        slots: &[HnswNode],
        id_of: &[u32],
        q: &[f64],
        ep: u32,
        layer: usize,
        ef: usize,
    ) -> Vec<(u32, f64)> {
        // visited: avoid re-expanding a node
        let mut visited = std::collections::BTreeSet::new();
        visited.insert(ep);

        let ep_dist = Self::dist_to(slots, ep, q);

        // c_heap: min-heap of (dist, slot) — candidates to expand
        let mut c_heap: BinaryHeap<Reverse<(OrdF64, u32)>> = BinaryHeap::new();
        c_heap.push(Reverse((OrdF64(ep_dist), ep)));

        // w_heap: max-heap of (dist, slot) — ef-best results (worst on top for eviction)
        let mut w_heap: BinaryHeap<(OrdF64, u32)> = BinaryHeap::new();
        w_heap.push((OrdF64(ep_dist), ep));

        while let Some(&Reverse((OrdF64(c_dist), c))) = c_heap.peek() {
            // furthest in result set
            let f_dist = w_heap.peek().map(|(OrdF64(d), _)| *d).unwrap_or(f64::MAX);
            if c_dist > f_dist {
                break; // all remaining candidates are farther than our worst result
            }
            c_heap.pop();

            let neighbors: &[u32] = slots
                .get(c as usize)
                .and_then(|n| n.layers.get(layer))
                .map(|l| l.as_slice())
                .unwrap_or_default();

            for &e in neighbors {
                if visited.contains(&e) {
                    continue;
                }
                if !Self::is_live(id_of, e) {
                    continue; // defensive: a freed slot is never a candidate
                }
                visited.insert(e);

                let e_dist = Self::dist_to(slots, e, q);
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

        w_heap.into_iter().map(|(OrdF64(d), s)| (s, d)).collect()
    }

    /// Greedy 1-NN descent from `ep` at `layer`. Returns the nearest slot
    /// found (used for upper-layer descent during insert/search).
    fn greedy_step(slots: &[HnswNode], id_of: &[u32], q: &[f64], ep: u32, layer: usize) -> u32 {
        let mut curr = ep;
        let mut curr_dist = Self::dist_to(slots, ep, q);
        loop {
            let mut improved = false;
            let neighbors: &[u32] = slots
                .get(curr as usize)
                .and_then(|n| n.layers.get(layer))
                .map(|l| l.as_slice())
                .unwrap_or_default();
            for &nb in neighbors {
                if !Self::is_live(id_of, nb) {
                    continue;
                }
                let d = Self::dist_to(slots, nb, q);
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
        note_insert();
        let Some(unit) = l2_normalize(v) else {
            return; // zero vector — skip
        };

        // Remove existing entry if any (handles update = remove + re-insert).
        if self.slot_of.contains_key(&id) {
            self.remove(id);
        }

        let level = gen_level(self.base_seed, id);

        // Allocate the slot up front. Nothing lists it yet, so beam search
        // cannot reach it; scoring it during the prune below then needs no
        // special case for "the node not in the graph yet".
        let slot = self.alloc_slot(
            id,
            HnswNode {
                level,
                vector: unit.clone(),
                layers: vec![vec![]; level + 1],
            },
        );

        let Some(ep) = self.entry_point else {
            // First node ever inserted.
            self.entry_point = Some(slot);
            self.max_level = level;
            return;
        };

        let max_level = self.max_level;
        let mut curr_ep = ep;

        // Phase 1: greedy descent from max_level to level+1 (ef=1).
        for lc in ((level + 1)..=max_level).rev() {
            curr_ep = Self::greedy_step(&self.slots, &self.id_of, &unit, curr_ep, lc);
        }

        // Phase 2: beam-search + connect at each layer from min(level, max_level)
        // down to 0.
        for lc in (0..=level.min(max_level)).rev() {
            let m_lc = if lc == 0 { M0 } else { M };

            // Beam search to collect ef_construction nearest candidates.
            let mut candidates = Self::beam_search(
                &self.slots,
                &self.id_of,
                &unit,
                curr_ep,
                lc,
                EF_CONSTRUCTION,
            );

            // Sort ascending by distance and take M nearest as neighbors.
            candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let neighbors: Vec<u32> = candidates.iter().take(m_lc).map(|(s, _)| *s).collect();
            self.set_layer(slot, lc, neighbors.clone());

            // Advance curr_ep to the nearest candidate.
            if let Some(&(nearest, _)) = candidates.first() {
                curr_ep = nearest;
            }

            // Add bidirectional links and prune over-connected neighbors.
            for &nb in &neighbors {
                let nb_node = &mut self.slots[nb as usize];
                while nb_node.layers.len() <= lc {
                    nb_node.layers.push(vec![]);
                }
                if !nb_node.layers[lc].contains(&slot) {
                    nb_node.layers[lc].push(slot);
                    self.link_back_ref(slot, nb);
                }

                // Prune if over-connected (simple selection: keep M nearest to nb).
                if self.slots[nb as usize].layers[lc].len() <= m_lc {
                    continue;
                }
                let nb_vec: Vec<f64> = self.slots[nb as usize].vector.clone();
                let current: Vec<u32> = self.slots[nb as usize].layers[lc].clone();

                // Score each current neighbor by distance to nb.
                let mut scored: Vec<(u32, f64)> = current
                    .iter()
                    .map(|&s| {
                        let d = if Self::is_live(&self.id_of, s) {
                            Self::dist_to(&self.slots, s, &nb_vec)
                        } else {
                            f64::MAX
                        };
                        (s, d)
                    })
                    .collect();

                scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                scored.truncate(m_lc);
                self.set_layer(nb, lc, scored.into_iter().map(|(s, _)| s).collect());
            }
        }

        // Update entry point if new node has a higher level.
        if level > max_level {
            self.entry_point = Some(slot);
            self.max_level = level;
        }
    }

    /// Remove node `id` from the index.
    ///
    /// Every reference to its slot is stripped — the slot is about to be reused
    /// by another node, so a leftover link would silently name the wrong
    /// vector. `back_refs` is what makes finding those references O(in-degree)
    /// rather than a scan of the whole index. If `id` was the entry point, a
    /// new entry point is elected (highest remaining level).
    pub fn remove(&mut self, id: u32) {
        let Some(slot) = self.slot_of.get(&id).copied() else {
            return;
        };

        // Drop the node's own out-edges, and with them its back-ref claims.
        // Done directly rather than through `set_layer` because every layer is
        // cleared at once: no target can still be listed on another layer.
        let mut targets: BTreeSet<u32> = BTreeSet::new();
        for layer in &self.slots[slot as usize].layers {
            targets.extend(layer.iter().copied());
        }
        for layer in self.slots[slot as usize].layers.iter_mut() {
            layer.clear();
        }
        for t in targets {
            if let Some(refs) = self.back_refs.get_mut(&t) {
                refs.remove(&slot);
                if refs.is_empty() {
                    self.back_refs.remove(&t);
                }
            }
        }

        // Walk every node that lists this slot and strip it. Asymmetric pruning
        // means these are NOT just the nodes it listed back, which is why the
        // reverse index exists.
        let referrers = self.referrers_of(slot);
        note_remove_scanned(referrers.len());
        for referrer in referrers {
            for layer in self.slots[referrer as usize].layers.iter_mut() {
                layer.retain(|&x| x != slot);
            }
        }
        self.clear_back_refs(slot);

        self.free_slot(slot);

        // Update entry point if needed.
        if self.entry_point == Some(slot) {
            if self.slot_of.is_empty() {
                self.entry_point = None;
                self.max_level = 0;
            } else {
                // Iterate in node-id order and keep the last maximum, exactly
                // as the BTreeMap scan this replaced did.
                let (_, &new_ep) = self
                    .slot_of
                    .iter()
                    .max_by_key(|(_, &s)| self.slots[s as usize].level)
                    .expect("slot_of is non-empty");
                self.entry_point = Some(new_ep);
                self.max_level = self.slots[new_ep as usize].level;
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
            curr_ep = Self::greedy_step(&self.slots, &self.id_of, &unit_q, curr_ep, lc);
        }

        // Beam search at layer 0 with ef candidates.
        let candidates = Self::beam_search(&self.slots, &self.id_of, &unit_q, curr_ep, 0, ef);

        // Convert slots → node ids and distances → cosine similarities; sort
        // descending; take k.
        let mut results: Vec<(u32, f64)> = candidates
            .into_iter()
            .map(|(s, dist)| (self.id_of[s as usize], (1.0 - dist).clamp(-1.0, 1.0)))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(k);
        results
    }

    // -----------------------------------------------------------------------
    // Test-only introspection
    // -----------------------------------------------------------------------

    /// The slots `back_refs` claims list `id`'s slot as a neighbour.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn back_refs_for_test(&self, id: u32) -> BTreeSet<u32> {
        let Some(&slot) = self.slot_of.get(&id) else {
            return BTreeSet::new();
        };
        self.back_refs.get(&slot).cloned().unwrap_or_default()
    }

    /// The same set, derived by walking every live node's adjacency lists.
    /// The oracle `back_refs_for_test` must agree with.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn scan_back_refs_for_test(&self, id: u32) -> BTreeSet<u32> {
        let Some(&slot) = self.slot_of.get(&id) else {
            return BTreeSet::new();
        };
        let mut out = BTreeSet::new();
        for (s, node) in self.slots.iter().enumerate() {
            if !Self::is_live(&self.id_of, s as u32) {
                continue;
            }
            if node.layers.iter().any(|l| l.contains(&slot)) {
                out.insert(s as u32);
            }
        }
        out
    }

    /// Number of adjacency slots ever allocated, live or freed. A removal that
    /// reuses a slot does not grow it.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn slot_capacity_for_test(&self) -> usize {
        self.slots.len()
    }
}

// ---------------------------------------------------------------------------
// Persisted blob: magic + version
// ---------------------------------------------------------------------------

/// Magic bytes at the head of a v2 (0.6.6) HNSW blob.
pub const HNSW_BLOB_MAGIC: [u8; 4] = *b"MHNS";
/// Highest blob version this build can read, and the one it writes.
pub const HNSW_BLOB_VERSION: u16 = 2;

/// On-disk wrapper for a persisted HNSW graph.
///
/// `magic` + `version` make a 0.6.5 blob and a 0.6.6 blob distinguishable
/// without bumping the snapshot format: section 6 carries the index as two
/// opaque `Vec<u8>` per rule, so only the bytes inside change.
///
/// The reverse direction is safe by construction: a 0.6.5 binary meeting one of
/// these fails its `bincode::deserialize::<HnswIndex>` against the old id-keyed
/// shape and falls back to the `hnsw_tracked` full scan — slower, never wrong.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HnswBlob {
    pub magic: [u8; 4],
    pub version: u16,
    pub index: HnswIndex,
}

/// Serialize-only twin of [`HnswBlob`] so encoding never clones the index.
#[derive(Serialize)]
struct HnswBlobRef<'a> {
    magic: [u8; 4],
    version: u16,
    index: &'a HnswIndex,
}

/// The 0.6.5 on-disk shape: node ids key the map *and* name the adjacency
/// lists. Read-only — nothing writes it any more.
#[derive(Deserialize)]
struct HnswIndexV1 {
    base_seed: u64,
    nodes: BTreeMap<u32, HnswNodeV1>,
    entry_point: Option<u32>,
    max_level: usize,
}

#[derive(Deserialize)]
struct HnswNodeV1 {
    level: usize,
    vector: Vec<f64>,
    /// Neighbour node **ids**.
    layers: Vec<Vec<u32>>,
}

/// Serialize `index` as a versioned blob. `None` only if bincode fails.
pub fn encode_hnsw_blob(index: &HnswIndex) -> Option<Vec<u8>> {
    bincode::serialize(&HnswBlobRef {
        magic: HNSW_BLOB_MAGIC,
        version: HNSW_BLOB_VERSION,
        index,
    })
    .ok()
}

/// Decode a persisted HNSW blob.
///
/// Accepts the v2 wrapper, and — for a store last written by 0.6.5 — a bare
/// bincoded `HnswIndex` in the old id-keyed shape, which is up-converted in
/// memory by remapping the decoded adjacency lists onto slots and deriving
/// `back_refs` from them. No distance is computed and no vector is re-inserted.
///
/// A blob whose magic matches but whose version this build does not know is
/// rejected exactly as a corrupt one is: the caller leaves the side on its
/// `hnsw_tracked` full-scan fallback rather than risk misreading it.
pub fn decode_hnsw_blob(blob: &[u8]) -> Result<HnswIndex, String> {
    if blob.is_empty() {
        return Err("empty blob".to_string());
    }
    match bincode::deserialize::<HnswBlob>(blob) {
        Ok(b) if b.magic == HNSW_BLOB_MAGIC => {
            if b.version == 0 || b.version > HNSW_BLOB_VERSION {
                return Err(format!(
                    "HNSW blob version {} is not readable by this build (reads up to \
                     {HNSW_BLOB_VERSION})",
                    b.version
                ));
            }
            let mut index = b.index;
            index.rebuild_back_refs();
            Ok(index)
        }
        // No v2 wrapper (or foreign magic): try the 0.6.5 shape.
        other => match bincode::deserialize::<HnswIndexV1>(blob) {
            Ok(v1) => Ok(HnswIndex::from_v1(v1)),
            Err(v1_err) => Err(match other {
                Ok(b) => format!(
                    "unrecognised HNSW blob magic {:?}, and not a v1 index either ({v1_err})",
                    b.magic
                ),
                Err(v2_err) => format!("not a v2 HNSW blob ({v2_err}) and not a v1 one ({v1_err})"),
            }),
        },
    }
}

// ---------------------------------------------------------------------------
// Archived search helper
// ---------------------------------------------------------------------------

/// Deserialize an `HnswIndex` from a bincoded blob and search it.
///
/// Returns an empty vec if the blob is corrupt or `q` is the zero vector.
/// Blobs are produced by `engine.rs` when it calls `bincode::serialize` on the
/// index before handing it to the V8 encoder.
///
/// # Caller note
///
/// This function has no current caller in the codebase.  It is a Task-3 /
/// future-use primitive: external callers with direct access to a V8 HNSW blob
/// (e.g. snapshot introspection tools or the upcoming MCP search path) can use
/// this to run an ANN query without a live `RuleEngine`.
pub fn search_hnsw_blob(blob: &[u8], q: &[f64], k: usize) -> Vec<(u32, f64)> {
    let Ok(idx) = decode_hnsw_blob(blob) else {
        return vec![];
    };
    idx.search(q, k)
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

    // -----------------------------------------------------------------------
    // Reverse adjacency
    // -----------------------------------------------------------------------

    /// `back_refs` must agree with a full scan of every adjacency list, through
    /// inserts, removes and re-inserts. This is the invariant that makes it
    /// safe to reuse a slot: if a stale reference survived a removal it would
    /// silently name whichever node took the slot over.
    #[test]
    fn back_refs_match_a_full_scan() {
        let vecs = make_unit_vecs(500, 32, 0x5EED_1234);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"backrefs"));
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }
        for i in (0..500).step_by(5) {
            idx.remove(i as u32);
        }
        for i in (0..500).step_by(5) {
            idx.insert(i as u32, &vecs[i]);
        }
        assert_eq!(idx.len(), 500, "every node is back in the index");
        for &id in idx.node_ids().iter() {
            assert_eq!(
                idx.back_refs_for_test(id),
                idx.scan_back_refs_for_test(id),
                "back_refs[{id}] disagrees with a full scan"
            );
        }
        // Freed slots were reused rather than appended to.
        assert_eq!(
            idx.slot_capacity_for_test(),
            500,
            "a remove + re-insert must recycle the slot"
        );
    }

    /// An insert that replaces an existing id is a remove plus an insert, and
    /// must leave the graph exactly as if the id had never been there.
    #[test]
    fn insert_then_remove_equals_never_inserted() {
        let vecs = make_unit_vecs(200, 16, 0xABCD_0001);
        let seed = crate::index::fnv1a_u64(b"rm-equiv");

        // Reference: ids 0..199 except 42, 77 and 150.
        let skipped = [42usize, 77, 150];
        let mut reference = HnswIndex::new(seed);
        for (i, v) in vecs.iter().enumerate() {
            if skipped.contains(&i) {
                continue;
            }
            reference.insert(i as u32, v);
        }

        // Subject: every id, then the three removed.
        let mut subject = HnswIndex::new(seed);
        for (i, v) in vecs.iter().enumerate() {
            subject.insert(i as u32, v);
        }
        for &i in &skipped {
            subject.remove(i as u32);
        }

        assert_eq!(
            subject.node_ids(),
            reference.node_ids(),
            "the removed ids must be gone from the index"
        );
        for &id in subject.node_ids().iter() {
            assert_eq!(
                subject.back_refs_for_test(id),
                subject.scan_back_refs_for_test(id),
                "back_refs[{id}] disagrees with a full scan after the removals"
            );
        }
        // A removed id is unreachable: query at its own vector and it must not
        // come back, however many neighbours are asked for.
        for &i in &skipped {
            let hits = subject.search(&vecs[i], 20);
            assert!(
                !hits.iter().any(|&(id, _)| id == i as u32),
                "removed id {i} is still reachable through the graph"
            );
        }
    }

    /// A removal must not touch every node in the index. Asserted on the
    /// operation count, not the wall clock: `remove` visits exactly the nodes
    /// that list the victim, and nothing else.
    #[test]
    fn remove_touches_only_the_nodes_that_list_it() {
        let vecs = make_unit_vecs(1_500, 32, 0xD00D_0007);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"rm-cost"));
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }

        let mut worst = 0u64;
        for i in (0..1_500).step_by(100) {
            let in_degree = idx.back_refs_for_test(i as u32).len() as u64;
            hnsw_remove_scanned_reset();
            idx.remove(i as u32);
            let scanned = hnsw_remove_scanned();
            assert_eq!(
                scanned, in_degree,
                "removing {i} visited {scanned} nodes for an in-degree of {in_degree}"
            );
            worst = worst.max(scanned);
        }
        let live = idx.len() as u64;
        assert!(
            worst * 2 < live,
            "worst removal touched {worst} of {live} live nodes — the O(N·M₀) scan \
             is still there"
        );
    }

    /// The wall-clock form of the same claim, for the record. Ignored by
    /// default because it builds a 5k index.
    ///
    /// Run with:
    /// `cargo test --release -p mushroomdb-rules -- remove_is_not_a_full_scan --ignored --nocapture`
    #[test]
    #[ignore = "slow: builds a 5k index"]
    fn remove_is_not_a_full_scan() {
        let vecs = make_unit_vecs(5_000, 64, 0xD00D);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"rm-cost"));
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }
        let t = std::time::Instant::now();
        for i in (0..5_000).step_by(100) {
            idx.remove(i as u32);
        }
        let mean = t.elapsed() / 50;
        eprintln!("mean removal at n=5000: {mean:?}");
        assert!(
            mean < std::time::Duration::from_millis(5),
            "mean removal {mean:?} at n=5000 — the O(N·M₀) scan is still there"
        );
    }

    // -----------------------------------------------------------------------
    // Blob magic and version
    // -----------------------------------------------------------------------

    /// The 0.6.5 on-disk shape, serialize side, so a test can write one.
    #[derive(Serialize)]
    struct V1Node {
        level: usize,
        vector: Vec<f64>,
        layers: Vec<Vec<u32>>,
    }

    #[derive(Serialize)]
    struct V1Index {
        base_seed: u64,
        nodes: BTreeMap<u32, V1Node>,
        entry_point: Option<u32>,
        max_level: usize,
    }

    /// Re-express a live index in the id-keyed 0.6.5 shape.
    fn as_v1_blob(idx: &HnswIndex) -> Vec<u8> {
        let nodes: BTreeMap<u32, V1Node> = idx
            .slot_of
            .iter()
            .map(|(&id, &s)| {
                let n = &idx.slots[s as usize];
                (
                    id,
                    V1Node {
                        level: n.level,
                        vector: n.vector.clone(),
                        layers: n
                            .layers
                            .iter()
                            .map(|l| l.iter().map(|&t| idx.id_of[t as usize]).collect())
                            .collect(),
                    },
                )
            })
            .collect();
        bincode::serialize(&V1Index {
            base_seed: idx.base_seed,
            nodes,
            entry_point: idx.entry_point.map(|s| idx.id_of[s as usize]),
            max_level: idx.max_level,
        })
        .unwrap()
    }

    fn blob_fixture() -> (Vec<Vec<f64>>, HnswIndex) {
        let vecs = make_unit_vecs(120, 24, 0x0B10_B0B0);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"blob-rt"));
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }
        (vecs, idx)
    }

    fn assert_matches(loaded: &HnswIndex, original: &HnswIndex, q: &[f64]) {
        assert_eq!(loaded.node_ids(), original.node_ids(), "node ids differ");
        for &id in loaded.node_ids().iter() {
            assert_eq!(
                loaded.back_refs_for_test(id),
                loaded.scan_back_refs_for_test(id),
                "back_refs[{id}] disagrees with a full scan after the load"
            );
        }
        assert_eq!(
            loaded.search(q, 10),
            original.search(q, 10),
            "the loaded graph answers differently"
        );
    }

    /// A blob written by 0.6.5 — a bare bincoded `HnswIndex` with id-keyed
    /// adjacency — is up-converted in place, with no vector re-inserted.
    #[test]
    fn a_v1_blob_upgrades_in_place() {
        let (vecs, idx) = blob_fixture();
        let blob = as_v1_blob(&idx);

        hnsw_insert_count_reset();
        let loaded = decode_hnsw_blob(&blob).expect("a 0.6.5 blob must still load");
        assert_eq!(
            hnsw_insert_count(),
            0,
            "up-converting a v1 blob must not re-insert a single vector"
        );
        assert_matches(&loaded, &idx, &vecs[3]);
    }

    /// The 0.6.6 wrapper round-trips.
    #[test]
    fn a_v2_blob_round_trips() {
        let (vecs, idx) = blob_fixture();
        let blob = encode_hnsw_blob(&idx).expect("encode");
        assert_eq!(
            &blob[..4],
            &HNSW_BLOB_MAGIC,
            "the blob must carry its magic"
        );

        hnsw_insert_count_reset();
        let loaded = decode_hnsw_blob(&blob).expect("a v2 blob must load");
        assert_eq!(hnsw_insert_count(), 0, "a load must not re-insert vectors");
        assert_matches(&loaded, &idx, &vecs[3]);
    }

    /// A version this build does not know is treated as corrupt, not guessed
    /// at. The caller's fallback is the full scan — slower, never wrong.
    #[test]
    fn an_unknown_version_is_rejected() {
        let (_, idx) = blob_fixture();
        let mut blob = encode_hnsw_blob(&idx).expect("encode");
        blob[4] = HNSW_BLOB_VERSION as u8 + 1; // bump the version's low byte
        let err = decode_hnsw_blob(&blob).expect_err("a future version must not be read");
        assert!(
            err.contains("version"),
            "the refusal must name the version: {err}"
        );
    }

    /// Foreign magic is rejected too, and does not fall through to a garbage
    /// v1 read.
    #[test]
    fn a_foreign_magic_is_rejected() {
        let (_, idx) = blob_fixture();
        let mut blob = encode_hnsw_blob(&idx).expect("encode");
        blob[0] = b'X';
        assert!(
            decode_hnsw_blob(&blob).is_err(),
            "a blob with foreign magic must not be read"
        );
    }

    /// The downgrade direction: a 0.6.5 binary meeting a 0.6.6 blob fails its
    /// `bincode::deserialize::<HnswIndex>` against the old shape rather than
    /// misreading it. `V1ReadIndex` is that old shape's read side.
    #[test]
    fn a_v2_blob_is_not_readable_as_a_v1_index() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct V1ReadNode {
            level: usize,
            vector: Vec<f64>,
            layers: Vec<Vec<u32>>,
        }
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct V1ReadIndex {
            base_seed: u64,
            nodes: BTreeMap<u32, V1ReadNode>,
            entry_point: Option<u32>,
            max_level: usize,
        }

        let (_, idx) = blob_fixture();
        let blob = encode_hnsw_blob(&idx).expect("encode");
        assert!(
            bincode::deserialize::<V1ReadIndex>(&blob).is_err(),
            "a 0.6.5 reader must reject a 0.6.6 blob, not misread it"
        );
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
            let approx_ids: Vec<usize> = idx
                .search(q, K)
                .into_iter()
                .map(|(id, _)| id as usize)
                .collect();
            let hits = approx_ids
                .iter()
                .filter(|id| exact_set.contains(id))
                .count();
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

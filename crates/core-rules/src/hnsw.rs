//! In-tree HNSW approximate nearest-neighbor index.
//!
//! Implements the Malkov & Yashunin (2018) Hierarchical Navigable Small World
//! algorithm, including the §3.5 diverse-neighbour heuristic (Algorithm 4) that
//! decides which links survive a prune. The index shape lives in
//! [`HnswParams`]; [`hnsw_params`] reads it once from the environment.
//!
//! Parameters are set for high-dimensional text embeddings (768-d to 2048-d).
//! At these dimensionalities the nearest-neighbour distribution is flat, so it
//! is neighbour *diversity* rather than neighbour *count* that lets the beam
//! route through the hierarchy and recover the true k-NN.
//!
//! All vectors are L2-normalized at insert time; cosine similarity reduces to
//! dot product for unit vectors, which is faster and numerically stable.
//!
//! **Determinism**, and its one sharp edge: the level assigned to each node is
//! derived from a seeded PRNG (`splitmix64`) seeded with
//! `FNV-1a(rule_name) XOR (node_id × PHI)`, so insertion order and seed fix the
//! levels. They do **not** fix the graph on their own — the edges also depend on
//! [`HnswParams`], which [`hnsw_params`] reads from `MUSHROOMDB_HNSW_PARAMS`.
//!
//! So: one WAL replayed by two processes with different `MUSHROOMDB_HNSW_PARAMS`
//! produces two *different* graphs. That is sound, because the graph is derived
//! state — every edge it yields is recomputable and the blob is never compared
//! byte-for-byte across replicas — but it does mean the variable belongs with
//! the deployment's configuration, not with a single node's environment. Set it
//! identically across replicas, or accept that their approximate answers differ
//! (each still above the recall floor its own shape was gated at).

use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

/// Maximum layer cap: prevents pathological depth on tiny graphs.
const MAX_LEVEL: usize = 16;

// ---------------------------------------------------------------------------
// Index shape
// ---------------------------------------------------------------------------

/// Index shape. Store-wide, not per rule: nothing asked to tune one rule
/// differently from another, and a per-rule parameter set would have to be
/// persisted in `RuleDef` and versioned with it.
///
/// Not persisted, either — the parameters bound how a graph is *built*, never
/// how a built graph is *read*, so a blob written under one shape is read
/// correctly under any other. That is why the v2 blob's meaning is unchanged by
/// this struct's existence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswParams {
    /// Max connections per layer above layer 0.
    pub m: usize,
    /// Max connections at layer 0.
    pub m0: usize,
    /// Beam width during insertion.
    pub ef_construction: usize,
    /// Beam width during search; the floor under a query's own `k`.
    pub ef_search: usize,
    /// How far the §3.5 diverse-neighbour heuristic reaches.
    pub prune: Prune,
}

/// Which side of a new link the §3.5 diverse-neighbour heuristic decides.
///
/// The heuristic always chooses the new node's **own** neighbours — that is
/// where diversity is cheap, because the candidate list is built once per
/// insert. Applying it a second time on the **neighbour** side, to re-decide
/// each over-connected neighbour's list, is what costs: that call is O(m₀²)
/// distance computations and it runs once per neighbour per insert.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Prune {
    /// Heuristic for the new node's own neighbours; keep-the-m-nearest on the
    /// neighbour side.
    ///
    /// **Below the recall floor on clustered corpora at the default `m0`**: it
    /// scores 0.8581 on `approximate_recall_5k_timing`, whose floor is 0.90. Opt
    /// in only with a raised `m0` or a corpus measured to tolerate it.
    ///
    /// The speed-up depends entirely on the corpus, so quote both numbers: **5×**
    /// on a raw 5,000 × 1,536-D index of uniformly random vectors (45.8 s against
    /// 251 s), but only **1.5×** on the clustered corpus the rule engine actually
    /// derives edges over (123.1 s against 180.5 s of backfill) — and that is the
    /// corpus where it misses the floor.
    Own,
    /// Heuristic on both sides — the paper's Algorithm 4 applied in full. The
    /// default, because it is the cheapest shape that passes every committed
    /// recall gate: see `docs/site/rules.md` for the three-way comparison
    /// against `own` and against raising `m0` instead.
    #[default]
    Both,
}

impl Prune {
    /// Parse the `prune` field of `MUSHROOMDB_HNSW_PARAMS`.
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "own" => Some(Self::Own),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

impl Default for HnswParams {
    /// The shipped shape, chosen by the recall gates (`hnsw_5k_1536_recall`,
    /// `approximate_recall_above_floor_1536dim_1k`, `approximate_recall_5k_timing`)
    /// and not by argument.
    ///
    /// `m` falls 32 → 16 and `m0` falls 128 → 64, because M₀ = 128 was standing
    /// in for the §3.5 diverse-neighbour heuristic the prune now applies; the
    /// prune's cost is quadratic in `m0`, so the per-insert constant falls with
    /// it. `ef_construction` falls 400 → 200 because it buys almost nothing once
    /// the neighbours are diverse.
    ///
    /// `ef_search` does **not** fall. At 1,536 dimensions the nearest-neighbour
    /// distribution is flat enough that recall is a beam-width problem, and
    /// `hnsw_5k_1536_recall` refused every narrower beam that was tried: at
    /// `ef_search` = 128 the min recall is 0.30 at `m0` = 32 and 0.70 at
    /// `m0` = 64, against a floor of 0.90.
    ///
    /// `prune` is [`Prune::Both`] because [`Prune::Own`], which builds 5× faster
    /// on uniform vectors but only 1.5× faster on clustered ones, scores **0.8581** on
    /// `approximate_recall_5k_timing` against a floor of 0.90 — that corpus's
    /// clusters are 100 members wide against an `m0` of 64, and the neighbour
    /// side is where those links get thrown away. The two ways to fix it are
    /// this and `m0` = 128; this one is both faster end to end (180 s against
    /// 269 s of backfill) and half the adjacency memory, so it is the default.
    fn default() -> Self {
        Self {
            m: 16,
            m0: 64,
            ef_construction: 200,
            ef_search: 400,
            prune: Prune::Both,
        }
    }
}

impl HnswParams {
    /// Parse `m,m0,ef_construction,ef_search[,prune]`. `None` if the shape is
    /// wrong, or a numeric field is absent, unparseable or zero — a zero would
    /// produce an index with no edges or a search with no beam. The `prune`
    /// field is optional and defaults to [`Prune::Both`]; it is the one field
    /// that is a word rather than a number, so it cannot be confused with the
    /// four ahead of it.
    fn parse(s: &str) -> Option<Self> {
        let fields: Vec<&str> = s.split(',').collect();
        if fields.len() < 4 || fields.len() > 5 {
            return None;
        }
        let mut num = fields
            .iter()
            .take(4)
            .map(|f| f.trim().parse::<usize>().ok().filter(|&v| v > 0));
        let mut next = || num.next().flatten();
        let (m, m0, ef_construction, ef_search) = (next()?, next()?, next()?, next()?);
        let prune = match fields.get(4) {
            Some(f) => Prune::parse(f)?,
            None => Prune::default(),
        };
        Some(Self {
            m,
            m0,
            ef_construction,
            ef_search,
            prune,
        })
    }
}

/// The index shape this process builds and searches with.
///
/// Read once from `MUSHROOMDB_HNSW_PARAMS`, formatted
/// `m,m0,ef_construction,ef_search[,prune]`, where `prune` is `own` or `both`
/// and defaults to `both`. Unset or unparseable →
/// [`HnswParams::default`]. Read once rather than per call so a graph cannot be
/// half-built under one shape and half under another, and so the value is a
/// pointer chase on the insert path.
///
/// For benchmarks and for an operator who has measured their own corpus; not a
/// per-rule knob.
pub fn hnsw_params() -> HnswParams {
    static PARAMS: std::sync::OnceLock<HnswParams> = std::sync::OnceLock::new();
    *PARAMS.get_or_init(|| {
        std::env::var("MUSHROOMDB_HNSW_PARAMS")
            .ok()
            .and_then(|s| HnswParams::parse(&s))
            .unwrap_or_default()
    })
}

/// `M` — max connections per layer (except layer 0).
#[deprecated(note = "read hnsw_params() instead")]
pub const M: usize = 16;
/// `M₀` — max connections at layer 0.
#[deprecated(note = "read hnsw_params() instead")]
pub const M0: usize = 64;
/// Beam width for insertion.
#[deprecated(note = "read hnsw_params() instead")]
pub const EF_CONSTRUCTION: usize = 200;
/// Beam width for search.
#[deprecated(note = "read hnsw_params() instead")]
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
    static HNSW_SEARCH_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count one vector actually indexed. Called *after* the zero-vector early
/// return, so the count is "vectors the graph took", not "calls made".
/// Compiles away without `test-hooks`.
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

/// Count one query answered by the graph itself (past the empty-index guards).
#[inline]
fn note_search() {
    #[cfg(any(test, feature = "test-hooks"))]
    HNSW_SEARCH_COUNT.with(|c| c.set(c.get().saturating_add(1)));
}

/// Nodes whose adjacency lists `HnswIndex::remove` has touched on this thread
/// since the last reset.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_remove_scanned() -> u64 {
    HNSW_REMOVE_SCANNED.with(|c| c.get())
}

/// Reset this thread's removal-scan counter to zero.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_remove_scanned_reset() {
    HNSW_REMOVE_SCANNED.with(|c| c.set(0));
}

/// Vectors indexed by any `HnswIndex` on this thread since the last reset.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_insert_count() -> u64 {
    HNSW_INSERT_COUNT.with(|c| c.get())
}

/// Reset this thread's indexed-vector counter to zero.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_insert_count_reset() {
    HNSW_INSERT_COUNT.with(|c| c.set(0));
}

/// Queries answered by an `HnswIndex` graph on this thread since the last
/// reset. Zero means every approximate query fell back to a full scan.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_search_count() -> u64 {
    HNSW_SEARCH_COUNT.with(|c| c.get())
}

/// Reset this thread's graph-query counter to zero.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_search_count_reset() {
    HNSW_SEARCH_COUNT.with(|c| c.set(0));
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
    let ml = 1.0 / (hnsw_params().m as f64).ln();
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
    /// Overrides [`hnsw_params`]'s `prune` for this index only.
    ///
    /// Exists because `hnsw_params()` reads the environment through a
    /// `OnceLock`: one process gets one shape, so a test that wants to compare
    /// both prune strategies cannot get there through the env. Compiled out
    /// entirely without `test-hooks`, and `#[serde(skip)]` regardless, so it can
    /// reach neither a production build nor a blob.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-hooks"))]
    #[serde(skip)]
    pub prune_override_for_test: Option<Prune>,
}

impl HnswIndex {
    /// The prune this index builds with: the per-index test override when one is
    /// set, otherwise the process-wide shape.
    #[inline]
    fn resolved_prune(&self, params: &HnswParams) -> Prune {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(p) = self.prune_override_for_test {
            return p;
        }
        params.prune
    }
}

/// A freed slot's id sentinel.
const DEAD: u32 = u32::MAX;

/// What an [`HnswIndex`] is holding, in countable units rather than bytes.
///
/// Produced by [`HnswIndex::memory_stats`]. Payload only: see that method for
/// what is deliberately not counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HnswMemoryStats {
    /// Vectors currently indexed.
    pub live_nodes: usize,
    /// Total neighbour entries across every live node's every layer.
    pub neighbour_slots: usize,
    /// Total entries in the reverse adjacency index.
    pub back_ref_entries: usize,
    /// Total `f64`s stored across every live node's vector.
    pub vector_floats: usize,
}

impl HnswMemoryStats {
    /// Payload bytes per indexed vector: adjacency (4 B per entry, forward and
    /// reverse) plus the vector itself (8 B per dimension). Zero for an empty
    /// index.
    pub fn bytes_per_node(&self) -> f64 {
        if self.live_nodes == 0 {
            return 0.0;
        }
        let bytes = (self.neighbour_slots + self.back_ref_entries) * 4 + self.vector_floats * 8;
        bytes as f64 / self.live_nodes as f64
    }

    /// The adjacency half of [`Self::bytes_per_node`] — the half the index
    /// shape controls. The vector half is fixed by the embedding's dimension.
    pub fn adjacency_bytes_per_node(&self) -> f64 {
        if self.live_nodes == 0 {
            return 0.0;
        }
        ((self.neighbour_slots + self.back_ref_entries) * 4) as f64 / self.live_nodes as f64
    }
}

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
            ..Self::default()
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

    /// The **first-rejection prune** — a short-cut of HNSW Algorithm 4, not
    /// Algorithm 4 itself. Read this before changing it: the difference is
    /// deliberate, measured, and visible in the graph.
    ///
    /// `candidates` must be `(slot, distance-to-base)` sorted nearest-first; the
    /// base vector itself is never needed again, only those distances. Walking
    /// nearest-first, a candidate is **kept** when it is closer to the base than
    /// to every neighbour already kept — a candidate sitting behind an
    /// already-kept neighbour is reachable *through* it, so the link would spend
    /// a slot without adding a route. That much is Algorithm 4's diversity test,
    /// and it is what `m0` = 128 was paying for before 0.6.6.
    ///
    /// **Where it stops being Algorithm 4.** Once `rejections ≥ candidates.len()
    /// − m`, every candidate still unseen is taken without testing it, and the
    /// walk ends. Consequences, both real:
    ///
    /// * The tail it takes is the **farthest** candidates, untested. Algorithm 4
    ///   would have gone on testing, and then filled any shortfall with the
    ///   **nearest** rejects (`keepPrunedConnections`). Those are different sets,
    ///   so this builds a different graph — it is a short-cut, not an
    ///   optimisation.
    /// * On the neighbour side the candidate list is always exactly `m + 1`
    ///   long, so `candidates.len() − m` is 1 and the walk stops at the **first**
    ///   rejection. That prune therefore performs exactly one diversity
    ///   rejection and keeps the rest as it found them, rather than re-deciding
    ///   the whole adjacency list.
    /// * Because the walk only ever ends with `kept.len() ≥ m` (or with the tail
    ///   taken), `keepPrunedConnections` would never fire. There is no backfill
    ///   here; if you restore the full algorithm you must restore it too, or
    ///   nodes will under-connect into inbound-only leaves that beam search
    ///   cannot route through.
    ///
    /// **Why the short-cut ships, and it is not only speed.** Full Algorithm 4
    /// was implemented and measured against this, both at `prune = both`,
    /// `m0` = 64:
    ///
    /// | gate | full Algorithm 4 | this |
    /// |---|---|---|
    /// | `hnsw_5k_1536_recall` (5 000 × 1 536-D uniform) | 1.0000 / 1.0000 in **369 s** | 1.0000 / 1.0000 in **254 s** |
    /// | `clustered_…_wider_than_m0` (40 × 120, 128-D) | min **0.5000** / mean 0.9725 | min **0.8000** / mean 0.9950 |
    /// | `approximate_recall_5k_timing` (5 000 clustered) | 1.0000, backfill **226.6 s** | 1.0000, backfill **181.5 s** |
    ///
    /// So the short-cut is 1.25–1.45× faster *and* strictly better on the corpus
    /// whose clusters are wider than `m0`. The reason is `keepPrunedConnections`
    /// itself: it backfills with the **nearest** rejects, and on a wide cluster
    /// those are all crowded in the one direction the diversity test just
    /// rejected. Taking the untested far tail instead keeps longer-range links,
    /// which is what makes the cluster reachable from outside. The paper's
    /// algorithm is the more principled one; on this corpus and at this `m0` it
    /// is measurably the worse one, which is why the deviation is deliberate
    /// rather than a bug to fix later. `docs/site/rules.md` carries the same
    /// table for operators.
    fn select_neighbors_first_rejection(
        slots: &[HnswNode],
        id_of: &[u32],
        candidates: &[(u32, f64)],
        m: usize,
    ) -> Vec<u32> {
        let mut kept: Vec<u32> = Vec::with_capacity(m);

        for (i, &(cand, d_base)) in candidates.iter().enumerate() {
            if kept.len() >= m {
                break;
            }
            // `rejections >= candidates.len() - m`, rearranged to avoid an
            // underflow when the list is shorter than `m`. Everything from here
            // on is taken untested: see the doc comment for what that costs.
            if kept.len() + (candidates.len() - i) <= m {
                kept.extend(
                    candidates[i..]
                        .iter()
                        .map(|&(c, _)| c)
                        .filter(|&c| Self::is_live(id_of, c)),
                );
                break;
            }
            if !Self::is_live(id_of, cand) {
                continue;
            }
            let cand_vec = &slots[cand as usize].vector;
            // Closer to the base than to anything already kept?
            let diverse = kept
                .iter()
                .all(|&k| d_base < Self::dist_to(slots, k, cand_vec));
            if diverse {
                kept.push(cand);
            }
            // A rejection is not recorded: nothing downstream reads it, because
            // there is no backfill. The `kept.len() + remaining <= m` test above
            // is the same condition as `rejections >= candidates.len() - m`.
        }
        debug_assert!(kept.len() <= m, "the prune must respect its allowance");
        kept
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
        let Some(unit) = l2_normalize(v) else {
            return; // zero vector — skip, and do not count it as indexed
        };
        note_insert();

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

        let params = hnsw_params();
        let prune = self.resolved_prune(&params);
        let max_level = self.max_level;
        let mut curr_ep = ep;

        // Phase 1: greedy descent from max_level to level+1 (ef=1).
        for lc in ((level + 1)..=max_level).rev() {
            curr_ep = Self::greedy_step(&self.slots, &self.id_of, &unit, curr_ep, lc);
        }

        // Phase 2: beam-search + connect at each layer from min(level, max_level)
        // down to 0.
        for lc in (0..=level.min(max_level)).rev() {
            let m_lc = if lc == 0 { params.m0 } else { params.m };

            // Beam search to collect ef_construction nearest candidates.
            let mut candidates = Self::beam_search(
                &self.slots,
                &self.id_of,
                &unit,
                curr_ep,
                lc,
                params.ef_construction,
            );

            sort_by_distance(&mut candidates);

            // Advance curr_ep to the nearest candidate before the heuristic
            // thins the list: the descent wants the nearest node, not the most
            // diverse one.
            if let Some(&(nearest, _)) = candidates.first() {
                curr_ep = nearest;
            }

            // The new node's own neighbours go through the heuristic too. This
            // is where the diversity buys recall: taking the m nearest leaves
            // every node in a dense cluster pointing back into the same
            // cluster, and the beam never crosses out of it.
            let neighbors =
                Self::select_neighbors_first_rejection(&self.slots, &self.id_of, &candidates, m_lc);
            self.set_layer(slot, lc, neighbors.clone());

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

                if self.slots[nb as usize].layers[lc].len() <= m_lc {
                    continue;
                }
                // Over-connected. `set_layer` keeps `back_refs` in step for
                // every link the prune drops, whichever prune that is.
                let nb_vec: Vec<f64> = self.slots[nb as usize].vector.clone();
                let current: Vec<u32> = self.slots[nb as usize].layers[lc].clone();
                let mut scored: Vec<(u32, f64)> = current
                    .iter()
                    .filter(|&&s| Self::is_live(&self.id_of, s))
                    .map(|&s| (s, Self::dist_to(&self.slots, s, &nb_vec)))
                    .collect();
                sort_by_distance(&mut scored);
                let kept: Vec<u32> = match prune {
                    // Keep the m nearest. One distance per candidate, already
                    // computed above.
                    Prune::Own => scored.iter().take(m_lc).map(|(s, _)| *s).collect(),
                    // Re-decide the whole list by diversity: O(m₀²) distances,
                    // once per over-connected neighbour per insert.
                    Prune::Both => Self::select_neighbors_first_rejection(
                        &self.slots,
                        &self.id_of,
                        &scored,
                        m_lc,
                    ),
                };
                self.set_layer(nb, lc, kept);
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
        let Some(ep) = self.entry_point.filter(|&s| Self::is_live(&self.id_of, s)) else {
            // No entry point, or one left dangling by a bug: answer nothing
            // rather than walk from a freed slot and hand back a `u32::MAX` id.
            return vec![];
        };
        if k == 0 {
            return vec![];
        }

        note_search();
        let ef = k.max(hnsw_params().ef_search);
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

    /// Count the graph's own storage, so a benchmark can report bytes per
    /// indexed vector instead of guessing at them.
    ///
    /// Counts payload only — the `u32`s in every adjacency list, the `u32`s in
    /// the reverse index, and the `f64`s of every vector. Allocator headers and
    /// the per-`Vec`/per-`BTreeSet` bookkeeping are not counted, so this is a
    /// floor on resident size, not a measurement of it.
    pub fn memory_stats(&self) -> HnswMemoryStats {
        let mut neighbour_slots = 0usize;
        let mut vector_floats = 0usize;
        for (s, node) in self.slots.iter().enumerate() {
            if !Self::is_live(&self.id_of, s as u32) {
                continue;
            }
            neighbour_slots += node.layers.iter().map(|l| l.len()).sum::<usize>();
            vector_floats += node.vector.len();
        }
        HnswMemoryStats {
            live_nodes: self.slot_of.len(),
            neighbour_slots,
            back_ref_entries: self.back_refs.values().map(|s| s.len()).sum(),
            vector_floats,
        }
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

/// Sort `(slot, distance)` pairs nearest-first, breaking ties on the slot so
/// the order is a function of the graph and not of heap iteration order. The
/// §3.5 heuristic reads this order, so a tie decided differently on two
/// machines would be two different graphs from the same WAL.
fn sort_by_distance(v: &mut [(u32, f64)]) {
    v.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
}

/// L2-normalize `v`. Returns `None` for the zero vector.
pub(crate) fn l2_normalize(v: &[f64]) -> Option<Vec<f64>> {
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm == 0.0 {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

/// Build `n` deterministic unit vectors in `dim` dimensions from a splitmix64
/// stream seeded with `seed`. Each vector is L2-normalized.
///
/// Public — and not `#[cfg(test)]` — so the in-crate recall gate, the
/// `tests/hnsw_scale.rs` benchmark and any external measurement all draw from
/// one generator: a recall number and a build time are only comparable when the
/// vectors behind them are the same vectors.
#[doc(hidden)]
pub fn make_unit_vecs(n: usize, dim: usize, seed: u64) -> Vec<Vec<f64>> {
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

/// Build `clusters × per_cluster` deterministic unit vectors, each a cluster
/// centre plus a small perturbation, in cluster order.
///
/// Uniformly random high-dimensional vectors are all nearly orthogonal, so they
/// exercise beam width and nothing else. Real embeddings cluster, and a cluster
/// wider than the layer-0 allowance is the case the §3.5 heuristic exists for:
/// without it every member of the cluster spends all its links inside the
/// cluster and the graph stops being navigable between clusters.
#[doc(hidden)]
pub fn make_clustered_unit_vecs(
    clusters: usize,
    per_cluster: usize,
    dim: usize,
    seed: u64,
) -> Vec<Vec<f64>> {
    let centres = make_unit_vecs(clusters, dim, seed);
    let mut state = seed ^ 0xA5A5_5A5A_1234_9876;
    let mut out = Vec::with_capacity(clusters * per_cluster);
    for centre in &centres {
        for _ in 0..per_cluster {
            let raw: Vec<f64> = centre
                .iter()
                .map(|c| {
                    state = splitmix64(state);
                    // A perturbation small enough that cluster membership is
                    // unambiguous, large enough that members are distinct.
                    c + 0.12 * (state as i64 as f64) / (i64::MAX as f64) / (dim as f64).sqrt()
                })
                .collect();
            out.push(l2_normalize(&raw).unwrap_or_else(|| centre.clone()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    // Parameters
    // -----------------------------------------------------------------------

    /// The deprecated constants are the default shape's fields. They are kept
    /// so an external reference still compiles, and this is what stops them
    /// drifting away from the values the index actually uses.
    #[test]
    #[allow(deprecated)]
    fn the_deprecated_constants_still_name_the_default_shape() {
        let d = HnswParams::default();
        assert_eq!(
            (M, M0, EF_CONSTRUCTION, EF_SEARCH),
            (d.m, d.m0, d.ef_construction, d.ef_search)
        );
    }

    /// The documented format, and every way of getting it wrong. A bad value
    /// falls back to the default rather than building a degenerate index: an
    /// `m0` of 0 is a graph with no edges, an `ef_search` of 0 is a beam with
    /// no width.
    #[test]
    fn params_parse_accepts_the_documented_format_and_nothing_else() {
        assert_eq!(
            HnswParams::parse("16,64,200,400"),
            Some(HnswParams::default())
        );
        assert_eq!(
            HnswParams::parse(" 8 , 64 , 300 , 96 "),
            Some(HnswParams {
                m: 8,
                m0: 64,
                ef_construction: 300,
                ef_search: 96,
                prune: Prune::Both
            }),
            "whitespace around a field must not defeat the override"
        );
        assert_eq!(
            HnswParams::parse("16,64,200,400,both"),
            Some(HnswParams::default()),
            "the prune field is optional, and `both` is what omitting it means"
        );
        assert_eq!(
            HnswParams::parse("16,64,200,400, own "),
            Some(HnswParams {
                prune: Prune::Own,
                ..HnswParams::default()
            }),
            "`own` opts out of the neighbour-side heuristic"
        );
        for bad in [
            "",
            "16",
            "16,64,200",
            "16,64,200,400,64",
            "16,64,200,400,neither",
            "16,64,200,400,own,own",
            "16,64,200,x",
            "0,64,200,400",
            "16,0,200,400",
            "16,64,0,400",
            "16,64,200,0",
            "-16,64,200,400",
        ] {
            assert_eq!(HnswParams::parse(bad), None, "{bad:?} must not parse");
        }
    }

    /// The prune strategy changes the graph, so it must change what the
    /// neighbour-side prune does — and nothing else. Both shapes must respect
    /// the degree bound and both must answer.
    #[test]
    fn both_prune_strategies_build_a_searchable_bounded_graph() {
        let params = hnsw_params();
        let vecs = make_unit_vecs(600, 24, 0x9121_5EED);
        for prune in [Prune::Own, Prune::Both] {
            let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"prune-shapes"));
            idx.prune_override_for_test = Some(prune);
            for (i, v) in vecs.iter().enumerate() {
                idx.insert(i as u32, v);
            }
            assert_eq!(idx.len(), 600, "{prune:?} lost nodes");
            for (s, node) in idx.slots.iter().enumerate() {
                if !HnswIndex::is_live(&idx.id_of, s as u32) {
                    continue;
                }
                for (lc, layer) in node.layers.iter().enumerate() {
                    let allowed = if lc == 0 { params.m0 } else { params.m };
                    assert!(
                        layer.len() <= allowed,
                        "{prune:?} slot {s} layer {lc} holds {} links for an allowance of \
                         {allowed}",
                        layer.len()
                    );
                }
            }
            let hits = idx.search(&vecs[42], 5);
            assert_eq!(
                hits.first().map(|&(id, _)| id),
                Some(42),
                "{prune:?} search"
            );
            for &id in idx.node_ids().iter() {
                assert_eq!(
                    idx.back_refs_for_test(id),
                    idx.scan_back_refs_for_test(id),
                    "{prune:?} back_refs[{id}] disagrees with a full scan"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // The §3.5 diverse-neighbour heuristic
    // -----------------------------------------------------------------------

    /// Build a throwaway index holding exactly `vecs`, with no edges, so a
    /// heuristic call can be made against known geometry.
    fn slots_of(vecs: &[Vec<f64>]) -> (Vec<HnswNode>, Vec<u32>) {
        let slots = vecs
            .iter()
            .map(|v| HnswNode {
                level: 0,
                vector: l2_normalize(v).unwrap(),
                layers: vec![vec![]],
            })
            .collect();
        let id_of = (0..vecs.len() as u32).collect();
        (slots, id_of)
    }

    /// The prune's whole point: given candidates at nearly the same distance,
    /// some of which sit behind a neighbour already kept, it keeps the ones that
    /// open a new direction. Sized so the diversity test genuinely runs rather
    /// than being short-circuited by the tail rule.
    #[test]
    fn the_prune_prefers_a_new_direction_over_a_redundant_neighbour() {
        // Six candidates, `m` = 2, so the tail short-cut cannot fire until four
        // rejections have been made: the diversity test runs at least three
        // times and its verdicts decide the answer. Candidate 0 opens one
        // direction; 1, 2 and 3 sit behind it at increasing distance; 4 opens a
        // second direction; 5 is far and behind 0. Keeping the m *nearest* would
        // answer [0, 1]; deleting the diversity test would also answer [0, 1].
        let base = l2_normalize(&[1.0, 0.0, 0.0]).unwrap();
        let vecs = vec![
            vec![0.80, 0.0, 0.60], // 0: nearest, direction A
            vec![0.78, 0.0, 0.63], // 1: behind 0
            vec![0.76, 0.0, 0.65], // 2: behind 0
            vec![0.74, 0.0, 0.67], // 3: behind 0
            vec![0.70, 0.71, 0.0], // 4: direction B — diverse
            vec![0.60, 0.0, 0.80], // 5: far, behind 0
        ];
        let (slots, id_of) = slots_of(&vecs);
        let mut cands: Vec<(u32, f64)> = (0..6u32)
            .map(|s| (s, HnswIndex::dist_to(&slots, s, &base)))
            .collect();
        sort_by_distance(&mut cands);
        assert_eq!(
            cands.iter().map(|&(s, _)| s).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5],
            "fixture: candidates must arrive in this nearest-first order"
        );

        let kept = HnswIndex::select_neighbors_first_rejection(&slots, &id_of, &cands, 2);
        assert_eq!(
            kept,
            vec![0, 4],
            "the prune must reject 1, 2 and 3 as reachable through 0 and keep 4, \
             which opens a direction 0 does not cover"
        );
    }

    /// The documented deviation from Algorithm 4, pinned so it cannot change by
    /// accident. Once `rejections >= candidates.len() - m` the prune takes the
    /// remaining candidates **untested** — the farthest ones — where Algorithm 4
    /// would keep testing and then backfill with the **nearest** reject. This
    /// fixture is a case where those two answers differ, and it asserts ours.
    #[test]
    fn the_prune_keeps_the_untested_tail_where_algorithm_4_would_backfill() {
        // `m` = 2 over three candidates, so one rejection triggers the tail.
        // 0 opens a direction; 1 is near but behind 0; 2 is far and behind 0.
        let base = l2_normalize(&[1.0, 0.0, 0.0]).unwrap();
        let vecs = vec![
            vec![0.80, 0.0, 0.60], // 0: nearest
            vec![0.78, 0.0, 0.63], // 1: behind 0, near
            vec![0.55, 0.0, 0.84], // 2: behind 0, far
        ];
        let (slots, id_of) = slots_of(&vecs);
        let mut cands: Vec<(u32, f64)> = (0..3u32)
            .map(|s| (s, HnswIndex::dist_to(&slots, s, &base)))
            .collect();
        sort_by_distance(&mut cands);
        assert_eq!(
            cands.iter().map(|&(s, _)| s).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "fixture: nearest-first order"
        );

        let kept = HnswIndex::select_neighbors_first_rejection(&slots, &id_of, &cands, 2);
        assert_eq!(
            kept,
            vec![0, 2],
            "the short-cut keeps the untested far candidate; full Algorithm 4 \
             would have rejected it too and backfilled with the nearer reject 1, \
             answering [0, 1]. If this ever reads [0, 1] the prune has become \
             Algorithm 4 and `select_neighbors_first_rejection`'s name, its doc \
             comment and docs/site/rules.md are all now wrong."
        );
    }

    /// A dead slot is never linked to. `insert` filters them before scoring,
    /// but the heuristic is the last gate before an adjacency list is written.
    #[test]
    fn the_prune_never_keeps_a_freed_slot() {
        let vecs = make_unit_vecs(4, 8, 0x7EA0_1234);
        let (slots, mut id_of) = slots_of(&vecs);
        id_of[1] = DEAD;
        let base = l2_normalize(&vecs[0]).unwrap();
        let mut cands: Vec<(u32, f64)> = (0..4u32)
            .map(|s| (s, HnswIndex::dist_to(&slots, s, &base)))
            .collect();
        sort_by_distance(&mut cands);
        let kept = HnswIndex::select_neighbors_first_rejection(&slots, &id_of, &cands, 4);
        assert!(
            !kept.contains(&1),
            "a freed slot must not become a neighbour"
        );
    }

    /// The memory win, guarded on every `cargo test`: degree bounds per layer
    /// and payload bytes per node against a stated ceiling.
    ///
    /// The whole argument for `m0` = 64 is that adjacency halves, and the 5,000 ×
    /// 1,536-D measurement that backs it is an `#[ignore]`d release run. This is
    /// the cheap version that actually runs: a regression that lets degree drift
    /// — a prune that stops pruning, a `set_layer` that leaks — shows up here
    /// first.
    #[test]
    fn degree_and_adjacency_bytes_stay_within_the_shape() {
        let params = hnsw_params();
        let vecs = make_unit_vecs(1_200, 32, 0xDE6E_E5EE);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"degree-bound"));
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }

        let mut worst_layer0 = 0usize;
        let mut worst_upper = 0usize;
        for (s, node) in idx.slots.iter().enumerate() {
            if !HnswIndex::is_live(&idx.id_of, s as u32) {
                continue;
            }
            for (lc, layer) in node.layers.iter().enumerate() {
                let allowed = if lc == 0 { params.m0 } else { params.m };
                assert!(
                    layer.len() <= allowed,
                    "slot {s} layer {lc} holds {} links for an allowance of {allowed}",
                    layer.len()
                );
                if lc == 0 {
                    worst_layer0 = worst_layer0.max(layer.len());
                } else {
                    worst_upper = worst_upper.max(layer.len());
                }
            }
        }

        // Payload per node. Forward entries are bounded by `m0` on layer 0 plus
        // `m` on each layer above, and the reverse index holds at most one entry
        // per (source, target) pair, so it can never exceed the forward count.
        // `2 * (m0 + m) * 4` bytes is therefore a true ceiling with room for the
        // upper layers, and it is tight enough to catch `m0` doubling.
        let mem = idx.memory_stats();
        let ceiling = 2.0 * (params.m0 + params.m) as f64 * 4.0;
        eprintln!(
            "degree: layer0 <= {worst_layer0} (allowance {}), upper <= {worst_upper} \
             (allowance {}); adjacency {:.1} B/node against a ceiling of {ceiling:.1}; \
             vector {:.1} B/node",
            params.m0,
            params.m,
            mem.adjacency_bytes_per_node(),
            (mem.vector_floats * 8) as f64 / mem.live_nodes as f64,
        );
        assert_eq!(mem.live_nodes, 1_200);
        assert!(
            mem.back_ref_entries <= mem.neighbour_slots,
            "the reverse index ({}) cannot hold more pairs than the forward one ({})",
            mem.back_ref_entries,
            mem.neighbour_slots
        );
        assert!(
            mem.adjacency_bytes_per_node() <= ceiling,
            "adjacency is {:.1} B/node against a ceiling of {ceiling:.1} — the \
             index shape grew",
            mem.adjacency_bytes_per_node()
        );
        // 32 f64s per vector, exactly, or the fixture is not what it says.
        assert_eq!(mem.vector_floats, 1_200 * 32);
    }

    /// Recall on a corpus whose clusters are wider than the layer-0 allowance.
    ///
    /// This is the case `M₀ = 128` was raised for in v0.4.2 and the case the
    /// §3.5 heuristic replaces it for: 40 clusters of 120 members each, against
    /// an `m0` of 64. Without diversity in the selection, every member of a
    /// cluster spends all 64 of its layer-0 links on other members of the same
    /// cluster, the cluster becomes a closed component, and a query that enters
    /// the graph elsewhere never reaches it.
    ///
    /// Queries are drawn from the same clustered distribution, so the top-10
    /// are inside one cluster and finding them means the beam must have got
    /// into that cluster.
    ///
    /// Run with:
    /// `cargo test --release -p mushroomdb-rules -- clustered_recall_survives_clusters_wider_than_m0 --ignored --nocapture`
    #[test]
    #[ignore = "slow: builds a 4,800-vector clustered index"]
    fn clustered_recall_survives_clusters_wider_than_m0() {
        const CLUSTERS: usize = 40;
        const PER_CLUSTER: usize = 120;
        const DIM: usize = 128;
        const K: usize = 10;

        assert!(
            PER_CLUSTER > hnsw_params().m0,
            "the fixture only proves anything when a cluster is wider than m0 \
             ({PER_CLUSTER} vs {})",
            hnsw_params().m0
        );

        let vecs = make_clustered_unit_vecs(CLUSTERS, PER_CLUSTER, DIM, 0xC1_05_7E_12_34_56_78_9A);

        // Queries: one member of each cluster, nudged. The true top-10 is then
        // that member and its nine nearest cluster-mates — a well-defined set,
        // unlike a query at the cluster centre, where 120 near-equidistant
        // members make "the top 10" a coin toss and recall measures noise.
        let queries: Vec<Vec<f64>> = (0..CLUSTERS)
            .map(|c| {
                let j = c * PER_CLUSTER + 17;
                let mut q = vecs[j].clone();
                q[0] += 1e-6;
                q
            })
            .collect();
        let exact: Vec<BTreeSet<usize>> = queries
            .iter()
            .map(|q| exact_knn(&vecs, q, K).into_iter().collect())
            .collect();

        // Both prune strategies, because this fixture is the only one that can
        // tell them apart, and the difference is the whole reason `Prune::Both`
        // exists as an option. Floors are per strategy: they are what was
        // measured, and the gap between them is the trade-off `rules.md`
        // documents.
        let mut scored: Vec<(Prune, f64, f64)> = Vec::new();
        for prune in [Prune::Own, Prune::Both] {
            let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"clustered-recall"));
            idx.prune_override_for_test = Some(prune);
            for (i, v) in vecs.iter().enumerate() {
                idx.insert(i as u32, v);
            }
            let recalls: Vec<f64> = queries
                .iter()
                .zip(exact.iter())
                .map(|(q, truth)| {
                    let found = idx
                        .search(q, K)
                        .into_iter()
                        .filter(|(id, _)| truth.contains(&(*id as usize)))
                        .count();
                    found as f64 / K as f64
                })
                .collect();
            let min = recalls.iter().cloned().fold(f64::MAX, f64::min);
            let mean = recalls.iter().sum::<f64>() / recalls.len() as f64;
            eprintln!(
                "clustered recall@{K} ({CLUSTERS}x{PER_CLUSTER}, dim {DIM}, m0={}, \
                 prune={prune:?}): min={min:.4} mean={mean:.4}",
                hnsw_params().m0
            );
            scored.push((prune, min, mean));
        }

        // Measured: Own min 0.5000 / mean 0.9350, Both min 0.8000 / mean 0.9950.
        // A query here sits inside a 120-member cluster whose members are
        // genuinely close together, so ranks 8 to 12 are separated by very
        // little and the tail of the top-10 is the hardest thing this index is
        // ever asked for. The mean says the beam reached the right cluster; the
        // min says no single query got stuck in one member's neighbourhood.
        for &(prune, min, mean) in &scored {
            let (min_floor, mean_floor) = match prune {
                Prune::Own => (0.40, 0.90),
                Prune::Both => (0.70, 0.95),
            };
            assert!(
                min >= min_floor,
                "{prune:?} min clustered recall@{K} = {min:.4} < {min_floor}"
            );
            assert!(
                mean >= mean_floor,
                "{prune:?} mean clustered recall@{K} = {mean:.4} < {mean_floor}"
            );
        }

        // The reason the option exists: on a corpus whose clusters are wider
        // than `m0`, the full heuristic must actually be better. If this ever
        // stops holding, `Prune::Both` is paying 5x the build time for nothing.
        let own = scored[0];
        let both = scored[1];
        assert!(
            both.1 >= own.1 && both.2 >= own.2,
            "Prune::Both (min {:.4} mean {:.4}) is not better than Prune::Own \
             (min {:.4} mean {:.4}) on clusters wider than m0 — the option has no \
             justification left",
            both.1,
            both.2,
            own.1,
            own.2
        );
    }

    /// Recall after churn. Insert 5,000 1,536-D vectors, remove a fifth,
    /// re-insert a tenth of them, remove a disjoint seventh, then measure
    /// recall@10 against brute force over exactly the surviving set. A graph
    /// that healed badly answers from the wrong neighbourhood; this is the test
    /// that would have caught a prune that orphaned nodes on removal.
    ///
    /// The corpus is the shape `hnsw_5k_1536_recall` uses, and for the same
    /// reason: `ef_search` is 400, so a graph of a few hundred reachable nodes
    /// is searched exhaustively and its recall is 1.0 whatever the churn did.
    /// Only at thousands of survivors does the number mean anything. That makes
    /// it a release gate rather than a `cargo test` one.
    ///
    /// Run with:
    /// `cargo test --release -p mushroomdb-rules -- recall_survives_insert_remove_churn --ignored --nocapture`
    #[test]
    #[ignore = "slow: builds a 5k x 1536-D index and churns it"]
    fn recall_survives_insert_remove_churn() {
        const N: usize = 5_000;
        const DIM: usize = 1_536;
        const K: usize = 10;

        let vecs = make_unit_vecs(N, DIM, 0xC0FF_EE00_5EED_1234);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"churn-recall"));
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }

        // Churn: drop every 5th, then bring back every 10th of those, then drop
        // a disjoint slice. Slots are reused throughout, so a stale adjacency
        // reference would now name the wrong vector.
        let mut live: BTreeSet<usize> = (0..N).collect();
        for i in (0..N).step_by(5) {
            idx.remove(i as u32);
            live.remove(&i);
        }
        for i in (0..N).step_by(10) {
            idx.insert(i as u32, &vecs[i]);
            live.insert(i);
        }
        for i in (3..N).step_by(7) {
            idx.remove(i as u32);
            live.remove(&i);
        }
        assert_eq!(
            idx.len(),
            live.len(),
            "the index and the oracle disagree on size"
        );

        // back_refs must still be exact — recall means nothing on a corrupt
        // graph. Spot-checked rather than verified for every node: the full
        // check is O(n² · m₀), which at n = 5,000 costs more than the whole
        // rest of this test, and `back_refs_match_a_full_scan` already runs it
        // exhaustively over a churned 500-node index on every `cargo test`.
        for &id in idx.node_ids().iter().step_by(53) {
            assert_eq!(
                idx.back_refs_for_test(id),
                idx.scan_back_refs_for_test(id),
                "back_refs[{id}] disagrees with a full scan after churn"
            );
        }

        // Ground truth over the survivors only.
        let survivors: Vec<usize> = live.iter().copied().collect();
        let queries = make_unit_vecs(40, DIM, 0x9111_0BED);
        let mut recalls = Vec::with_capacity(queries.len());
        for q in &queries {
            let mut scored: Vec<(usize, f64)> = survivors
                .iter()
                .map(|&i| {
                    let dot: f64 = vecs[i].iter().zip(q.iter()).map(|(a, b)| a * b).sum();
                    (i, dot)
                })
                .collect();
            scored.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.cmp(&b.0))
            });
            let exact: BTreeSet<usize> = scored.into_iter().take(K).map(|(i, _)| i).collect();

            let hits = idx.search(q, K);
            for (id, _) in &hits {
                assert!(
                    live.contains(&(*id as usize)),
                    "search returned {id}, which was removed"
                );
            }
            let found = hits
                .iter()
                .filter(|(id, _)| exact.contains(&(*id as usize)))
                .count();
            recalls.push(found as f64 / K as f64);
        }
        let min = recalls.iter().cloned().fold(f64::MAX, f64::min);
        let mean = recalls.iter().sum::<f64>() / recalls.len() as f64;
        eprintln!(
            "churn recall@{K}: min={min:.4} mean={mean:.4} over {} survivors",
            live.len()
        );
        // The same floor `hnsw_5k_1536_recall` holds the un-churned graph to.
        // Everything here is seeded, so these are fixed numbers, not a sample.
        assert!(min >= 0.90, "min recall@{K} after churn = {min:.4} < 0.90");
        assert!(
            mean >= 0.95,
            "mean recall@{K} after churn = {mean:.4} < 0.95"
        );
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

    /// Removing the entry point — the node that owns the top layer — must
    /// re-elect a live one and leave the graph searchable. A dangling entry
    /// point would either panic in `dist_to` or hand back a freed slot's id.
    #[test]
    fn removing_the_entry_point_re_elects_and_still_searches() {
        let vecs = make_unit_vecs(300, 16, 0xE47E_9001);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"ep-churn"));
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }

        // Peel off the entry point repeatedly: each removal must re-elect the
        // highest-level node still present, and the top layer eventually
        // collapses onto a lower one.
        let mut seen_levels = Vec::new();
        for _ in 0..12 {
            let ep_slot = idx
                .entry_point
                .expect("a non-empty index has an entry point");
            let ep_id = idx.id_of[ep_slot as usize];
            assert_ne!(ep_id, DEAD, "the entry point must name a live node");
            seen_levels.push(idx.max_level);

            idx.remove(ep_id);

            let new_ep = idx.entry_point.expect("re-election must find a live node");
            assert!(
                HnswIndex::is_live(&idx.id_of, new_ep),
                "the re-elected entry point is a freed slot"
            );
            assert_ne!(new_ep, ep_slot, "the removed slot is still the entry point");
            assert_eq!(
                idx.max_level, idx.slots[new_ep as usize].level,
                "max_level must follow the re-elected entry point"
            );
            let highest = idx
                .slot_of
                .values()
                .map(|&s| idx.slots[s as usize].level)
                .max()
                .unwrap();
            assert_eq!(
                idx.max_level, highest,
                "the entry point must be a highest-level node"
            );
            assert!(
                !idx.node_ids().contains(&ep_id),
                "the old entry point lingers"
            );

            // The graph still answers, and never with a removed id.
            let hits = idx.search(&vecs[200], 5);
            assert!(
                !hits.is_empty(),
                "the graph stopped answering after re-election"
            );
            for (id, _) in &hits {
                assert!(
                    idx.node_ids().contains(id),
                    "search returned {id}, which is not in the index"
                );
            }
            for &id in idx.node_ids().iter() {
                assert_eq!(
                    idx.back_refs_for_test(id),
                    idx.scan_back_refs_for_test(id),
                    "back_refs[{id}] disagrees with a full scan after an entry-point removal"
                );
            }
        }
        assert!(
            seen_levels.iter().any(|&l| l > 0),
            "the fixture never had a multi-layer entry point, so this proved nothing"
        );

        // Drain it entirely: the last removal must clear the entry point.
        for id in idx.node_ids() {
            idx.remove(id);
        }
        assert!(idx.is_empty());
        assert_eq!(idx.entry_point, None);
        assert_eq!(idx.max_level, 0);
        assert!(idx.search(&vecs[0], 5).is_empty());
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

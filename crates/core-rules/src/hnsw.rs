//! In-tree HNSW approximate nearest-neighbor index.
//!
//! Implements the Malkov & Yashunin (2018) Hierarchical Navigable Small World
//! algorithm, including a first-rejection short-cut of the §3.5 diverse-neighbour heuristic (not Algorithm 4; see `select_neighbors_first_rejection`) that
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
//! The index keeps its own copy of every vector as `f32` in one contiguous
//! [`VecSlab`] addressed by slot — the store keeps the `f64`s — and the dot
//! product is [`dot_f32`], summed in eight independent accumulators so the
//! compiler can emit parallel lanes. The index's distances choose *candidates*;
//! every score a caller sees is recomputed from the `f64` properties, so the
//! narrower type costs a tie-break and nothing observable. One slab means one
//! stride, which is why an embedding whose dimension differs from the first one
//! indexed is skipped rather than truncated.
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
    static HNSW_DIST_EVALS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static HNSW_DIST_EVALS_PAIRWISE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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

/// Count one distance evaluation. Called from the two distance functions and
/// nowhere else, so the count is "dot products the graph asked for".
///
/// One thread-local add per `dim` multiply-adds is under 0.1 % of the kernel at
/// any dimension worth indexing, and a counter a benchmark cannot read is not a
/// gate. Compiles away without `test-hooks`.
#[inline]
fn note_dist() {
    #[cfg(any(test, feature = "test-hooks"))]
    HNSW_DIST_EVALS.with(|c| c.set(c.get().saturating_add(1)));
}

/// Count one distance evaluation between two *indexed* vectors — the §3.5
/// prune's diversity test and the neighbour-side scoring loop. Always
/// accompanied by a [`note_dist`], so pairwise is a subset of the total and
/// `total − pairwise` is the beam plus the descent.
#[inline]
fn note_dist_pairwise() {
    #[cfg(any(test, feature = "test-hooks"))]
    HNSW_DIST_EVALS_PAIRWISE.with(|c| c.set(c.get().saturating_add(1)));
}

/// Distance evaluations on this thread since the last reset.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_dist_evals() -> u64 {
    HNSW_DIST_EVALS.with(|c| c.get())
}

/// The subset of [`hnsw_dist_evals`] that compared two indexed vectors rather
/// than a vector against a query.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_dist_evals_pairwise() -> u64 {
    HNSW_DIST_EVALS_PAIRWISE.with(|c| c.get())
}

/// Reset both distance-evaluation counters to zero.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn hnsw_dist_evals_reset() {
    HNSW_DIST_EVALS.with(|c| c.set(0));
    HNSW_DIST_EVALS_PAIRWISE.with(|c| c.set(0));
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
// The distance kernel
// ---------------------------------------------------------------------------

/// The index's own copy of every vector: one contiguous `f32` allocation, `dim`
/// floats per slot, addressed by slot.
///
/// The store keeps `f64`. This copy exists only to **choose candidates** —
/// `index.rs::hnsw_candidates` throws away the similarity a search returns and
/// keeps the ids, and every score a rule or a query reports is recomputed from
/// the `f64` properties. So `f32` here costs a tie-break at the 1e-7 level and
/// nothing a caller can observe, and it buys half the bytes and twice the lanes.
///
/// One slab rather than one `Vec<f64>` per node also means a distance is a
/// pointer offset into a known stride instead of a chase through an
/// independently allocated 12 KB block, and that the pair-distance path needs no
/// copy at all.
///
/// `dim` is 0 until the first vector arrives, and fixed from then on — with one
/// correction: an election made on a *single* sample can be undone, because the
/// first vector indexed may be the odd one out (see [`HnswIndex::insert`]).
///
/// `data` grows through `Vec::resize`, so its capacity grows geometrically: a
/// slab can hold up to about twice the bytes its rows need, transiently, and
/// [`HnswIndex::memory_stats`] does not see that — it counts rows, which is why
/// it documents itself as a floor on resident size. One amortised doubling is
/// the price of not copying the whole slab on every insert.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct VecSlab {
    dim: usize,
    data: Vec<f32>,
}

impl VecSlab {
    /// The row for `slot`, or an empty slice when the slab has no row there.
    /// Empty is unreachable for a live slot — every `alloc_slot` writes one —
    /// and returning it rather than panicking keeps a corrupt blob from taking
    /// the process down.
    #[inline]
    fn get(&self, slot: u32) -> &[f32] {
        if self.dim == 0 {
            return &[];
        }
        let start = slot as usize * self.dim;
        self.data.get(start..start + self.dim).unwrap_or(&[])
    }

    /// Write `v` into `slot`'s row, converting from the caller's unit `f64`
    /// vector. Fixes `dim` on the first call. `false` — nothing written — when
    /// `v.len()` disagrees with an established `dim`.
    fn put(&mut self, slot: u32, v: &[f64]) -> bool {
        if self.dim == 0 {
            self.dim = v.len();
        }
        if self.dim == 0 || v.len() != self.dim {
            return false;
        }
        let start = slot as usize * self.dim;
        let end = start + self.dim;
        if self.data.len() < end {
            self.data.resize(end, 0.0);
        }
        for (d, s) in self.data[start..end].iter_mut().zip(v) {
            *d = *s as f32;
        }
        true
    }

    /// Floats the slab holds for `live` nodes — an element count, so the
    /// caller decides what a float costs.
    #[inline]
    fn floats_for(&self, live: usize) -> usize {
        live * self.dim
    }
}

/// Dot product of two equal-length `f32` slices, summed in **eight independent
/// accumulators**.
///
/// IEEE addition is not associative, so LLVM may not reassociate a single
/// accumulator: `a.iter().zip(b).map(|(x, y)| x * y).sum()` is a serial chain of
/// `dim` dependent multiply-adds, `dim` × the 3–4 cycle latency of one add. At
/// 1,536 dimensions that is ~1.5 µs of a machine that could have done the work
/// in a fraction of it. Choosing the summation order here — eight partial sums
/// over `chunks_exact(8)`, which hands LLVM a known-length slice — is what lets
/// it emit parallel FMAs (NEON is baseline on `aarch64`; SSE2 on x86-64, which
/// is 4-wide multiply and add without FMA and so a smaller win).
///
/// `std::simd` would say this declaratively and is nightly-only; the toolchain
/// is pinned stable, so this is the portable way to say it. No `unsafe`, no
/// `cfg`, one code path.
#[inline]
fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "a dot product needs equal lengths");
    let mut acc = [0.0f32; 8];
    let mut ca = a.chunks_exact(8);
    let mut cb = b.chunks_exact(8);
    for (x, y) in ca.by_ref().zip(cb.by_ref()) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    let tail: f32 = ca
        .remainder()
        .iter()
        .zip(cb.remainder())
        .map(|(x, y)| x * y)
        .sum();
    acc.iter().sum::<f32>() + tail
}

/// Cosine distance between the two vectors in `a` and `b` — the prune's
/// diversity test and the neighbour-side scoring loop. Both are already unit, so
/// this is `1 − dot`, clamped to [0, 2].
///
/// Replaces the 12 KB `nb_vec.clone()` the neighbour-side prune used to make
/// once per over-connected neighbour per insert.
#[inline]
fn dist_slots(slab: &VecSlab, a: u32, b: u32) -> f64 {
    note_dist();
    note_dist_pairwise();
    let dot = dot_f32(slab.get(a), slab.get(b)) as f64;
    (1.0 - dot.clamp(-1.0, 1.0)).max(0.0)
}

/// Cosine distance from the vector in `slot` to the already-unit `f32` query
/// `q`. Returns `1 - dot` clamped to [0, 2] (0 = identical, 2 = opposite).
#[inline]
fn dist_to(slab: &VecSlab, slot: u32, q: &[f32]) -> f64 {
    note_dist();
    let dot = dot_f32(slab.get(slot), q) as f64;
    (1.0 - dot.clamp(-1.0, 1.0)).max(0.0)
}

/// Build a slab from vectors decoded out of an older blob, `vectors[slot]` being
/// the vector for that slot.
///
/// The stride is the first non-empty vector's length — a freed slot decodes as an
/// empty one, and so does a node a 0.6.5 writer left without a vector. A vector
/// of some *other* non-zero length is a mixed-dimension index, which older
/// builds accepted and whose distances they computed over the shorter of the two
/// vectors; it is copied as far as it goes and zero-filled beyond, because the
/// node is already in the graph and dropping it would leave adjacency naming a
/// slot that is not there. One log line names how many.
fn slab_of_decoded(vectors: &[Vec<f64>]) -> VecSlab {
    let dim = vectors
        .iter()
        .map(|v| v.len())
        .find(|&l| l != 0)
        .unwrap_or(0);
    let mut slab = VecSlab {
        dim,
        data: vec![0.0; vectors.len() * dim],
    };
    if dim == 0 {
        return slab;
    }
    let mut odd = 0usize;
    for (slot, v) in vectors.iter().enumerate() {
        if v.len() == dim {
            slab.put(slot as u32, v);
            continue;
        }
        if v.is_empty() {
            continue; // a freed slot: its row stays zero and nothing reads it
        }
        odd += 1;
        let start = slot * dim;
        for (d, s) in slab.data[start..start + dim].iter_mut().zip(v) {
            *d = *s as f32;
        }
    }
    if odd > 0 {
        eprintln!(
            "mushroomdb: HNSW loaded {odd} node(s) whose embedding is not {dim} \
             dimensions; they were padded to the index's stride. Re-embed the \
             collection with one model — their distances were already meaningless."
        );
    }
    slab
}

/// The `f32` form of a unit `f64` vector — the query shape every search path
/// uses, and bit-for-bit what [`VecSlab::put`] stored.
#[inline]
fn as_f32(v: &[f64]) -> Vec<f32> {
    v.iter().map(|&x| x as f32).collect()
}

// ---------------------------------------------------------------------------
// Node storage
// ---------------------------------------------------------------------------

/// A node's place in the hierarchy. The vector lives in the index's [`VecSlab`],
/// addressed by the same slot, which is why this struct has no vector field and
/// why the blob is version 3.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct HnswNode {
    /// Assigned layer level (inclusive; node has layers 0..=level).
    level: usize,
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
    /// Every indexed vector, `f32`, addressed by the same slot as `slots`.
    ///
    /// A freed slot keeps its row until another node takes the slot over: the
    /// bytes stay resident where a per-node `Vec` would have freed them. In
    /// exchange the index loses one allocator header and one fragmentation risk
    /// per node, and `memory_stats` counts live nodes only, so the figure it
    /// reports stays a floor on resident size rather than a measurement of it.
    slab: VecSlab,
    /// Vectors **refused** because their dimension disagreed with the slab's
    /// settled stride. Any refusal means the index is missing a vector it was
    /// offered, so [`HnswIndex::can_answer`] stops claiming the fast path — the
    /// caller's exhaustive scan is the correct answer and this one is not.
    ///
    /// A re-elected stride (see [`HnswIndex::insert`]) **evicts** rather than
    /// refuses, and does not count here: after it the index holds every vector
    /// it was offered at the stride it now has.
    ///
    /// Not persisted: a loaded index has refused nothing, and the vectors it
    /// holds are whatever the writer put in it. The first refusal on an index
    /// logs one line, and later ones are silent, so an ingest pointed at the
    /// wrong model cannot print a line per node.
    #[serde(skip)]
    dim_mismatches: u64,
    /// Vectors **evicted** by a stride re-election, kept `(id, unit vector)` so
    /// that a later re-election to their dimension can put them back.
    ///
    /// A re-election drops the one vector standing behind the old stride, and
    /// that vector may be the *real* corpus: in the order
    /// `[real, stray, real, …]` the first real vector is evicted by the stray and
    /// the stray is evicted by the second real one. Parking is what makes the
    /// first case recoverable — the second real vector re-elects that dimension
    /// and the parked vector is re-inserted — and [`Self::can_answer`] is what
    /// makes the gap safe while it lasts.
    ///
    /// Bounded by construction: a re-election needs the index to hold at most one
    /// vector, so this cannot grow with the corpus. Never persisted, and holding
    /// an `f64` copy of a vector the index is not indexing, which
    /// [`Self::memory_stats`] does not count.
    #[serde(skip)]
    parked: Vec<(u32, Vec<f64>)>,
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
    /// Total floats stored across every live node's vector. `f32`s since 0.6.6:
    /// the index's own copy is the slab's, and the `f64`s stay in the store.
    pub vector_floats: usize,
}

impl HnswMemoryStats {
    /// Payload bytes per indexed vector: adjacency (4 B per entry, forward and
    /// reverse) plus the index's own copy of the vector (4 B per dimension — the
    /// store's `f64` copy is not the index's business). Zero for an empty index.
    pub fn bytes_per_node(&self) -> f64 {
        if self.live_nodes == 0 {
            return 0.0;
        }
        let bytes = (self.neighbour_slots + self.back_ref_entries) * 4
            + self.vector_floats * std::mem::size_of::<f32>();
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

    /// **Ask this before searching.** True when the index can answer a query of
    /// `q_len` dimensions *completely* — meaning a caller may use its answer
    /// instead of an exhaustive scan.
    ///
    /// Three things have to hold, and each of them is a way the index can be
    /// useless rather than wrong:
    ///
    /// * It holds something. An empty index answers nothing.
    /// * Its stride **is** the query's dimension. A slab has one stride, so an
    ///   index of 1,536-D vectors cannot compare a 3-D query to anything, and —
    ///   the case that matters — an index that elected a 3-D stride from a stray
    ///   first vector cannot answer the 1,536-D queries the rule actually makes.
    /// * It has refused nothing (`dim_mismatches == 0`). A refused vector is one
    ///   the caller asked to index and the index does not hold, so its candidate
    ///   set is incomplete and the caller's own scan is the correct answer.
    /// * Nothing of `q_len` dimensions is **parked** — evicted by a stride
    ///   re-election and not yet put back. A re-election revives every parked
    ///   vector of the dimension it elects, so this clause should always hold for
    ///   the current stride; it is the assertion that makes that a guarantee
    ///   rather than a claim. A parked vector of some *other* dimension is not a
    ///   gap in this answer: it is not comparable with anything at this stride,
    ///   by the same rule (`def.rs`'s `VectorSimilar` refuses a pair of unequal
    ///   length) that makes it unable to be an edge.
    ///
    /// When this is false the caller must take its exhaustive path —
    /// `index.rs::hnsw_candidates` returns `hnsw_tracked`, and
    /// `engine.rs::hnsw_search_dst`/`_any_dst` return `None` so
    /// `db.rs::find_similar_vector` brute-forces. Correct and slower beats fast
    /// and short.
    pub fn can_answer(&self, q_len: usize) -> bool {
        !self.is_empty()
            && self.dim_mismatches == 0
            && self.slab.dim == q_len
            && !self.parked.iter().any(|(_, v)| v.len() == q_len)
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

    /// Bind `id` to a slot (reusing a freed one when available), store `node`
    /// there and write `unit` into the slab's row for it. The caller owns linking
    /// it into the graph, and must already have checked that `unit` matches the
    /// slab's dimension — `debug_assert`ed here, because a refused write would
    /// leave a live slot pointing at another node's stale row.
    fn alloc_slot(&mut self, id: u32, node: HnswNode, unit: &[f64]) -> u32 {
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
        let written = self.slab.put(slot, unit);
        debug_assert!(written, "alloc_slot was handed a vector the slab refused");
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
        let mut vectors: Vec<Vec<f64>> = Vec::with_capacity(id_of.len());
        let slots: Vec<HnswNode> = v1
            .nodes
            .into_values()
            .map(|n| {
                vectors.push(n.vector);
                HnswNode {
                    level: n.level,
                    layers: n
                        .layers
                        .into_iter()
                        .map(|l| l.iter().filter_map(|id| slot_of.get(id).copied()).collect())
                        .collect(),
                }
            })
            .collect();

        let mut out = Self {
            base_seed: v1.base_seed,
            entry_point: v1.entry_point.and_then(|e| slot_of.get(&e).copied()),
            max_level: v1.max_level,
            slab: slab_of_decoded(&vectors),
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

    /// Adopt a decoded 0.6.6-pre-3b (blob v2) index: the slot layout is already
    /// this one's, so only the vectors move — out of the per-node `Vec<f64>`s and
    /// into the slab, converted and nothing else. No distance is computed and no
    /// vector is re-inserted, so the graph is adopted exactly as it was built.
    fn from_v2(v2: HnswIndexV2) -> Self {
        let mut vectors: Vec<Vec<f64>> = Vec::with_capacity(v2.slots.len());
        let slots: Vec<HnswNode> = v2
            .slots
            .into_iter()
            .map(|n| {
                vectors.push(n.vector);
                HnswNode {
                    level: n.level,
                    layers: n.layers,
                }
            })
            .collect();
        let mut out = Self {
            base_seed: v2.base_seed,
            slab: slab_of_decoded(&vectors),
            slots,
            slot_of: v2.slot_of,
            id_of: v2.id_of,
            free: v2.free,
            entry_point: v2.entry_point,
            max_level: v2.max_level,
            ..Self::default()
        };
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
    ///
    /// The slab's row is left as it is: nothing reads a dead slot's vector
    /// (`is_live` gates every search path), and the next occupant overwrites it.
    /// The cost is that the row stays resident until then.
    fn free_slot(&mut self, slot: u32) {
        let id = std::mem::replace(&mut self.id_of[slot as usize], DEAD);
        self.slot_of.remove(&id);
        self.slots[slot as usize] = HnswNode::default();
        self.free.push(slot);
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Beam search on a single layer.
    ///
    /// Returns a list of `(slot, cosine_distance)` — the `ef` nearest
    /// candidates found starting from `ep`. Ascending distance order is not
    /// guaranteed (callers sort as needed).
    fn beam_search(
        slots: &[HnswNode],
        id_of: &[u32],
        slab: &VecSlab,
        q: &[f32],
        ep: u32,
        layer: usize,
        ef: usize,
    ) -> Vec<(u32, f64)> {
        // visited: avoid re-expanding a node. A `Vec<bool>` indexed by slot, not
        // a `BTreeSet`: this is probed once per candidate edge — order 10⁴ times
        // per insert — and each probe was an O(log V) chase through separately
        // allocated tree nodes. One memset per call buys O(1) probes. It changes
        // neither which nodes are expanded nor the order they are pushed in,
        // which is what keeps the graph a function of the WAL.
        let mut visited = vec![false; slots.len()];
        if let Some(v) = visited.get_mut(ep as usize) {
            *v = true;
        }

        let ep_dist = dist_to(slab, ep, q);

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
                if visited.get(e as usize).copied().unwrap_or(true) {
                    continue;
                }
                if !Self::is_live(id_of, e) {
                    continue; // defensive: a freed slot is never a candidate
                }
                visited[e as usize] = true;

                let e_dist = dist_to(slab, e, q);
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
        slab: &VecSlab,
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
            // Closer to the base than to anything already kept?
            let diverse = kept.iter().all(|&k| d_base < dist_slots(slab, k, cand));
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
    fn greedy_step(
        slots: &[HnswNode],
        id_of: &[u32],
        slab: &VecSlab,
        q: &[f32],
        ep: u32,
        layer: usize,
    ) -> u32 {
        let mut curr = ep;
        let mut curr_dist = dist_to(slab, ep, q);
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
                let d = dist_to(slab, nb, q);
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
    ///
    /// A vector whose dimension differs from the one this index settled on is
    /// **skipped too**, and the first such skip is logged. The slab has one
    /// stride, so there is nowhere to put it; before 0.6.6 the distance
    /// `zip`-truncated to the shorter of the two vectors and produced a number
    /// that meant nothing, which is a worse answer than no answer. A skip makes
    /// [`Self::can_answer`] false for good, so the rule falls back to its
    /// exhaustive scan rather than answering from an index that is missing a
    /// vector.
    ///
    /// **The stride is re-elected when it was elected from a single sample, and
    /// the vector it displaces is parked rather than lost.**
    ///
    /// The first vector an index takes sets the stride, and that vector may be
    /// the odd one out — one 3-element stray ingested ahead of a corpus of
    /// 1,536-D embeddings would otherwise refuse every real vector and leave a
    /// non-empty index that can answer nothing. So when the index holds at most
    /// one vector and the incoming one disagrees with it, the stride is
    /// re-elected to the incoming dimension, and the one vector standing behind
    /// the old stride is **evicted and parked**: removed from the graph, kept as
    /// `(id, unit vector)` in [`Self::parked`], and re-inserted the moment a
    /// re-election elects its dimension again.
    ///
    /// Parking is not a detail. In the order `[real, stray, real, …]` the first
    /// real vector is evicted by the stray, and the stray is evicted by the second
    /// real one — so without parking the first real vector would be gone for good
    /// from an index that believes itself complete, and the rule would silently
    /// lose its edges. With it, the second real vector's re-election puts the
    /// first one back, and [`Self::can_answer`] refuses the fast path for any
    /// dimension still sitting in the parked list.
    ///
    /// An eviction is not a refusal. A refusal discards a vector the index has no
    /// record of; an eviction keeps it, so the index can say precisely what it is
    /// missing and stop claiming only that.
    pub fn insert(&mut self, id: u32, v: &[f64]) {
        let Some(unit) = l2_normalize(v) else {
            return; // zero vector — skip, and do not count it as indexed
        };
        // An explicit insert supersedes any parked copy of the same node: the
        // caller is telling us this node's vector, and a stale parked one must
        // never be revived over it.
        self.parked.retain(|(pid, _)| *pid != id);
        if self.slab.dim != 0 && unit.len() != self.slab.dim {
            // At most one vector in, so the stride was elected on a sample of
            // one — or on a node that has since been removed, leaving a stride
            // with nothing behind it. Either way the election is not evidence
            // against the incoming vector: re-elect, park the single earlier
            // vector if there is one, and bring back anything parked at the
            // dimension now being elected.
            if self.len() <= 1 {
                let was = self.slab.dim;
                if let Some((&evicted, &slot)) = self.slot_of.iter().next() {
                    // From the slab rather than from the caller's original `f64`:
                    // an `f32` widened to `f64` is exact, so this is the vector
                    // the index was holding.
                    let kept: Vec<f64> = self.slab.get(slot).iter().map(|&x| x as f64).collect();
                    eprintln!(
                        "mushroomdb: HNSW re-elected its embedding dimension from {was} to {} \
                         at node {id}, and parked node {evicted}: the first vector indexed \
                         set the dimension and was the odd one out. Node {evicted} returns \
                         to the index if {was} dimensions are elected again; until then \
                         this index answers {was}-dimension queries through the full scan.",
                        unit.len()
                    );
                    self.remove(evicted);
                    self.parked.push((evicted, kept));
                }
                self.slab = VecSlab::default();
                self.slab.dim = unit.len();
                // Whatever was parked at this dimension belongs in the index
                // again. Their own inserts cannot re-enter this branch — their
                // length is the stride — so the recursion is one level deep.
                let mut revive: Vec<(u32, Vec<f64>)> = Vec::new();
                self.parked.retain(|entry| {
                    if entry.1.len() == unit.len() {
                        revive.push(entry.clone());
                        false
                    } else {
                        true
                    }
                });
                for (pid, pv) in revive {
                    self.insert(pid, &pv);
                }
            } else {
                self.dim_mismatches += 1;
                if self.dim_mismatches == 1 {
                    eprintln!(
                        "mushroomdb: HNSW skipped node {id}: its embedding has {} dimensions \
                         and this index holds {}. A mixed-dimension index cannot be \
                         searched, so this index will now answer through the full scan \
                         instead; re-embed the collection with one model. Further skips \
                         on this index are silent.",
                        unit.len(),
                        self.slab.dim
                    );
                }
                return;
            }
        }
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
                layers: vec![vec![]; level + 1],
            },
            &unit,
        );

        // The query every distance on this insert path is taken against: the
        // same `f32` values the slab now holds for `slot`, so a distance to the
        // new node is exactly a distance between two slab rows. Owned rather
        // than borrowed from the slab because the graph is mutated below.
        let q = as_f32(&unit);

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
            curr_ep = Self::greedy_step(&self.slots, &self.id_of, &self.slab, &q, curr_ep, lc);
        }

        // Phase 2: beam-search + connect at each layer from min(level, max_level)
        // down to 0.
        for lc in (0..=level.min(max_level)).rev() {
            let m_lc = if lc == 0 { params.m0 } else { params.m };

            // Beam search to collect ef_construction nearest candidates.
            let mut candidates = Self::beam_search(
                &self.slots,
                &self.id_of,
                &self.slab,
                &q,
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
                Self::select_neighbors_first_rejection(&self.slab, &self.id_of, &candidates, m_lc);
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
                //
                // Scoring is two slab rows per candidate — where 0.6.5 cloned
                // the neighbour's whole 12 KB vector first, once per
                // over-connected neighbour and so up to `m0` times per insert.
                let current: Vec<u32> = self.slots[nb as usize].layers[lc].clone();
                let mut scored: Vec<(u32, f64)> = current
                    .iter()
                    .filter(|&&s| Self::is_live(&self.id_of, s))
                    .map(|&s| (s, dist_slots(&self.slab, s, nb)))
                    .collect();
                sort_by_distance(&mut scored);
                let kept: Vec<u32> = match prune {
                    // Keep the m nearest. One distance per candidate, already
                    // computed above.
                    Prune::Own => scored.iter().take(m_lc).map(|(s, _)| *s).collect(),
                    // Re-decide the whole list by diversity: O(m₀²) distances,
                    // once per over-connected neighbour per insert.
                    Prune::Both => Self::select_neighbors_first_rejection(
                        &self.slab,
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
        // A removed node must not come back through a later stride re-election.
        // Done before the early return, because the node may be parked rather
        // than indexed — which is exactly the state a removal has to clear.
        self.parked.retain(|(pid, _)| *pid != id);
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
    /// sorted descending by similarity.
    ///
    /// **This method never falls back.** Zero-norm queries return empty, and so
    /// does a query whose dimension is not this index's stride — there is
    /// nothing to compare it against here. The fall-back to an exhaustive scan
    /// belongs to the caller, and [`Self::can_answer`] is how a caller knows it
    /// is needed: ask it first, and take your own path when it says no. Every
    /// caller in this workspace does (`index.rs::hnsw_candidates`,
    /// `engine.rs::hnsw_search_dst`/`_any_dst`).
    ///
    /// **The similarity is for ordering, not for reporting.** It is computed
    /// from the index's `f32` copies, so it is accurate to ~1e-6 and an exact
    /// duplicate scores 0.9999999 rather than 1.0. `hnsw_candidates` discards it
    /// and keeps the ids; `db.rs::find_similar_vector` re-scores every candidate
    /// against the `f64` property vectors before it applies `min`, orders, or
    /// reports anything. A new caller must do the same.
    pub fn search(&self, q: &[f64], k: usize) -> Vec<(u32, f64)> {
        self.search_with_ef(q, k, self.ef_for(k))
    }

    /// The beam width [`HnswIndex::search`] uses for `k` results.
    ///
    /// Exposed so a caller that widens the beam itself — an exact
    /// `VectorSimilar` rule looking for *every* hit above a floor — can start
    /// from the same place `search` would have. Reads [`hnsw_params`], so the
    /// widening loop in `index.rs` inherits whatever shape this process was
    /// configured with and never names a constant of its own.
    pub fn ef_for(&self, k: usize) -> usize {
        k.max(hnsw_params().ef_search)
    }

    /// [`HnswIndex::search`], with the layer-0 beam width set independently of
    /// the result count.
    ///
    /// `ef` below `k` is raised to `k`: a beam narrower than the answer cannot
    /// produce the answer.
    pub fn search_with_ef(&self, q: &[f64], k: usize, ef: usize) -> Vec<(u32, f64)> {
        let Some(unit_q) = l2_normalize(q) else {
            return vec![];
        };
        if self.slab.dim == 0 || unit_q.len() != self.slab.dim {
            return vec![];
        }
        let Some(ep) = self.entry_point.filter(|&s| Self::is_live(&self.id_of, s)) else {
            // No entry point, or one left dangling by a bug: answer nothing
            // rather than walk from a freed slot and hand back a `u32::MAX` id.
            return vec![];
        };
        if k == 0 {
            return vec![];
        }

        note_search();
        // The caller's width, floored at `k`: a beam narrower than the answer
        // cannot produce the answer. `search` passes `ef_for(k)`, which is the
        // `hnsw_params()` width this function used before the width became a
        // parameter, so its behaviour is unchanged.
        let ef = ef.max(k);
        let mut curr_ep = ep;
        let unit_q = as_f32(&unit_q);

        // Greedy descent from max_level to layer 1.
        for lc in (1..=self.max_level).rev() {
            curr_ep = Self::greedy_step(&self.slots, &self.id_of, &self.slab, &unit_q, curr_ep, lc);
        }

        // Beam search at layer 0 with ef candidates.
        let candidates = Self::beam_search(
            &self.slots,
            &self.id_of,
            &self.slab,
            &unit_q,
            curr_ep,
            0,
            ef,
        );

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
    /// the reverse index, and the `f32`s of every live node's slab row.
    /// Allocator headers and the per-`Vec`/per-`BTreeSet` bookkeeping are not
    /// counted, and neither are the slab rows of freed slots, so this is a floor
    /// on resident size, not a measurement of it.
    pub fn memory_stats(&self) -> HnswMemoryStats {
        let mut neighbour_slots = 0usize;
        for (s, node) in self.slots.iter().enumerate() {
            if !Self::is_live(&self.id_of, s as u32) {
                continue;
            }
            neighbour_slots += node.layers.iter().map(|l| l.len()).sum::<usize>();
        }
        let live_nodes = self.slot_of.len();
        HnswMemoryStats {
            live_nodes,
            neighbour_slots,
            back_ref_entries: self.back_refs.values().map(|s| s.len()).sum(),
            vector_floats: self.slab.floats_for(live_nodes),
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

/// Magic bytes at the head of every versioned (0.6.6 and later) HNSW blob.
pub const HNSW_BLOB_MAGIC: [u8; 4] = *b"MHNS";
/// Highest blob version this build can read, and the one it writes.
///
/// **3** since the distance kernel: the index no longer holds `f64` vectors per
/// node, it holds one `f32` slab, so the serialized shape changed and the blob
/// halved. Version 2 (0.6.6 before the kernel) and the bare 0.6.5 shape both
/// still decode, with no vector re-inserted and no distance computed. A reader
/// older than this one meeting a v3 blob fails its version check and leaves the
/// side on its `hnsw_tracked` full scan — slower, never wrong.
pub const HNSW_BLOB_VERSION: u16 = 3;

/// Bytes of the wrapper's header: `magic` (4 raw bytes) then `version` (a
/// little-endian `u16`) under bincode's fixed-int encoding. Read directly rather
/// than through a deserialize, because the rest of the wrapper's shape depends on
/// the version it carries.
const HNSW_BLOB_HEADER_LEN: usize = 6;

/// On-disk wrapper for a persisted HNSW graph.
///
/// `magic` + `version` make a 0.6.5 blob and a 0.6.6 blob distinguishable
/// without bumping the snapshot format: section 6 carries the index as two
/// opaque `Vec<u8>` per rule, so only the bytes inside change.
///
/// The reverse direction is safe by construction: an older binary meeting one of
/// these either fails its version check or fails its
/// `bincode::deserialize::<HnswIndex>` against the shape it knows, and falls
/// back to the `hnsw_tracked` full scan — slower, never wrong.
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

/// The blob-v2 shape — 0.6.6 before the distance kernel. Slot-keyed already, but
/// with an `f64` vector inside every node instead of a slab beside them.
/// Read-only: nothing writes it any more.
///
/// The field order is the v2 `HnswIndex`'s, and it must stay that way: bincode is
/// positional, so this struct *is* the old format's definition.
#[derive(Deserialize)]
struct HnswIndexV2 {
    base_seed: u64,
    slots: Vec<HnswNodeV2>,
    slot_of: BTreeMap<u32, u32>,
    id_of: Vec<u32>,
    free: Vec<u32>,
    entry_point: Option<u32>,
    max_level: usize,
}

#[derive(Deserialize)]
struct HnswNodeV2 {
    level: usize,
    vector: Vec<f64>,
    /// Neighbour **slots**, as in v3.
    layers: Vec<Vec<u32>>,
}

/// The v2 wrapper: the same magic and version, a different index shape.
#[derive(Deserialize)]
struct HnswBlobV2 {
    #[allow(dead_code)]
    magic: [u8; 4],
    #[allow(dead_code)]
    version: u16,
    index: HnswIndexV2,
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
/// Accepts, in this order:
///
/// * **v3** — this build's shape, slot-keyed with an `f32` slab.
/// * **v2** — 0.6.6 before the distance kernel, slot-keyed with an `f64` vector
///   per node. The vectors move into a slab and nothing else changes.
/// * **v1** — a bare bincoded `HnswIndex` from 0.6.5, id-keyed, up-converted by
///   remapping the decoded adjacency lists onto slots.
///
/// No distance is computed and no vector is re-inserted on any of those paths:
/// the graph is adopted as it was built.
///
/// The version is read from the header rather than inferred from a successful
/// deserialize, because v2 and v3 differ *inside* the wrapper: a v2 blob fed to
/// v3's shape could in principle decode into nonsense rather than fail. A blob
/// whose magic matches but whose version this build does not know is rejected
/// exactly as a corrupt one is — the caller leaves the side on its
/// `hnsw_tracked` full-scan fallback rather than risk misreading it.
pub fn decode_hnsw_blob(blob: &[u8]) -> Result<HnswIndex, String> {
    if blob.is_empty() {
        return Err("empty blob".to_string());
    }
    if blob.len() >= HNSW_BLOB_HEADER_LEN && blob[..4] == HNSW_BLOB_MAGIC {
        let version = u16::from_le_bytes([blob[4], blob[5]]);
        return match version {
            3 => bincode::deserialize::<HnswBlob>(blob)
                .map_err(|e| format!("HNSW v3 blob did not decode ({e})"))
                .map(|b| {
                    let mut index = b.index;
                    index.rebuild_back_refs();
                    index
                }),
            2 => bincode::deserialize::<HnswBlobV2>(blob)
                .map_err(|e| format!("HNSW v2 blob did not decode ({e})"))
                .map(|b| HnswIndex::from_v2(b.index)),
            v => Err(format!(
                "HNSW blob version {v} is not readable by this build (reads up to \
                 {HNSW_BLOB_VERSION})"
            )),
        };
    }
    // No wrapper, or foreign magic: try the 0.6.5 shape.
    match bincode::deserialize::<HnswIndexV1>(blob) {
        Ok(v1) => Ok(HnswIndex::from_v1(v1)),
        Err(v1_err) => Err(format!(
            "not a versioned HNSW blob (magic {:?}) and not a v1 one ({v1_err})",
            &blob[..4.min(blob.len())]
        )),
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

    /// The distance kernel is the insert path's whole cost, so the count of
    /// evaluations is the measurement this task is judged on. A counter a test
    /// cannot read is not a gate.
    #[test]
    fn a_single_insert_reports_its_distance_evaluations() {
        let vecs = make_unit_vecs(201, 32, 0x0D15_7A17);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"dist-evals"));
        for (i, v) in vecs.iter().take(200).enumerate() {
            idx.insert(i as u32, v);
        }

        hnsw_dist_evals_reset();
        idx.insert(200, &vecs[200]);
        let total = hnsw_dist_evals();
        let pairwise = hnsw_dist_evals_pairwise();
        assert!(
            total > 0,
            "an insert into a 200-node index must evaluate distances"
        );
        assert!(
            pairwise > 0,
            "the prune compares candidates against each other, so some \
             evaluations must be pairwise"
        );
        assert!(
            pairwise <= total,
            "pairwise ({pairwise}) is a subset of total ({total})"
        );

        // A one-node index: the only node the beam can reach is the entry point,
        // so a search scores it once per layer it descends through plus once in
        // the beam, and none of those evaluations is pairwise.
        let mut one = HnswIndex::new(7);
        one.insert(0, &vecs[0]);
        hnsw_dist_evals_reset();
        assert_eq!(one.search(&vecs[1], 5).len(), 1);
        assert_eq!(
            hnsw_dist_evals(),
            1 + one.max_level as u64,
            "a search of a one-node graph scores the entry point once per \
             descended layer and once in the beam"
        );
        assert_eq!(
            hnsw_dist_evals_pairwise(),
            0,
            "a search compares the query against nodes, never two nodes"
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
    // The slab and the kernel
    // -----------------------------------------------------------------------

    /// The index's own copy of a vector is one `f32` per dimension, in a slab
    /// indexed by slot — half the bytes of the `f64` store and one contiguous
    /// allocation rather than one per node. And because the slab has a single
    /// stride, a vector of some other dimension cannot be stored in it at all:
    /// it is skipped, where the `zip`-truncating distance of 0.6.5 would have
    /// taken it and produced meaningless distances.
    #[test]
    fn the_index_holds_one_f32_per_dimension() {
        const DIM: usize = 64;
        let vecs = make_unit_vecs(300, DIM, 0x51AB_1234);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"slab"));
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }

        let mem = idx.memory_stats();
        assert_eq!(mem.live_nodes, 300);
        assert_eq!(
            mem.vector_floats,
            300 * DIM,
            "the slab holds one float per dimension per live node"
        );
        let expected = mem.adjacency_bytes_per_node() + (DIM * 4) as f64;
        assert!(
            (mem.bytes_per_node() - expected).abs() < 1.0,
            "bytes per node is {:.1}, expected {expected:.1} = adjacency + {DIM} f32s",
            mem.bytes_per_node()
        );

        // A vector of another dimension cannot go in: the slab's stride is the
        // dimension of the first vector indexed.
        let odd = make_unit_vecs(1, DIM - 1, 0x0DD);
        hnsw_insert_count_reset();
        idx.insert(1_000, &odd[0]);
        assert_eq!(idx.len(), 300, "a 63-D vector must not enter a 64-D index");
        assert_eq!(
            hnsw_insert_count(),
            0,
            "a skipped vector must not be counted as indexed"
        );
        assert!(
            idx.search(&odd[0], 5).is_empty(),
            "a query of the wrong dimension cannot be answered"
        );

        // ...and one of the right dimension still can.
        let more = make_unit_vecs(1, DIM, 0xF00D);
        idx.insert(1_001, &more[0]);
        assert_eq!(idx.len(), 301, "a 64-D vector is still accepted");
        assert_eq!(hnsw_insert_count(), 1);
    }

    /// The first vector indexed elects the stride, and it may be the odd one
    /// out. One 3-element stray ahead of a real corpus must not void the index:
    /// the stride is re-elected and the stray evicted, and the index stays the
    /// fast path because it then holds everything it was offered at that stride.
    #[test]
    fn a_stray_first_vector_re_elects_the_stride() {
        const DIM: usize = 64;
        let vecs = make_unit_vecs(50, DIM, 0x5712_A140);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"re-elect"));

        // The stray arrives first and elects 3.
        idx.insert(900, &[1.0, 2.0, 3.0]);
        assert_eq!(idx.len(), 1);
        assert!(idx.can_answer(3), "a 3-D index can answer a 3-D query");
        assert!(!idx.can_answer(DIM), "...and not a 64-D one");

        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }

        assert_eq!(idx.len(), 50, "every real vector must be indexed");
        assert!(
            !idx.node_ids().contains(&900),
            "the stray must have been evicted, not kept"
        );
        assert!(
            idx.can_answer(DIM),
            "an index that re-elected its stride has refused nothing and must \
             stay the fast path"
        );
        assert!(!idx.can_answer(3), "the old stride is gone");
        assert_eq!(
            idx.search(&vecs[7], 1).first().map(|&(id, _)| id),
            Some(7),
            "and it must still answer correctly"
        );
        for &id in idx.node_ids().iter() {
            assert_eq!(
                idx.back_refs_for_test(id),
                idx.scan_back_refs_for_test(id),
                "back_refs[{id}] disagrees with a full scan after the eviction"
            );
        }

        // A stride with nothing behind it is not evidence either: drain the
        // index and it will take whatever dimension arrives next. Re-ingesting a
        // collection under a new embedding model must not need a new rule.
        for id in idx.node_ids() {
            idx.remove(id);
        }
        let sevens = make_unit_vecs(3, 7, 0x5E7E_0007);
        for (i, v) in sevens.iter().enumerate() {
            idx.insert(i as u32, v);
        }
        assert_eq!(idx.len(), 3, "an emptied index re-elects its stride");
        assert!(idx.can_answer(7) && !idx.can_answer(DIM));
    }

    /// The order that broke the first attempt at re-election: a real vector, a
    /// stray, then the rest of the corpus. The stray's re-election displaces the
    /// first real vector, and the second real vector's re-election displaces the
    /// stray — so the first one has to come back, and until it does the index
    /// must not claim it can answer for its dimension.
    #[test]
    fn a_real_vector_evicted_by_a_stray_comes_back() {
        const DIM: usize = 8;
        let vecs = make_unit_vecs(6, DIM, 0x0E01_C7ED);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"parked"));

        idx.insert(0, &vecs[0]);
        assert!(idx.can_answer(DIM));

        // The stray re-elects to 3 and parks node 0.
        idx.insert(900, &[1.0, 2.0, 3.0]);
        assert_eq!(idx.node_ids(), [900].into_iter().collect());
        assert!(
            !idx.can_answer(DIM),
            "while an 8-D vector is parked the index must not answer 8-D queries \
             — that is the window in which node 0 is missing"
        );

        // The next real vector re-elects to 8, parks the stray, and brings node 0
        // back.
        idx.insert(1, &vecs[1]);
        assert_eq!(
            idx.node_ids(),
            [0, 1].into_iter().collect(),
            "the vector the stray displaced must be re-inserted"
        );
        assert!(
            idx.can_answer(DIM),
            "with nothing of this dimension parked the index is complete again"
        );
        assert!(
            !idx.can_answer(3),
            "the stray's dimension is not the stride"
        );

        for (i, v) in vecs.iter().enumerate().skip(2) {
            idx.insert(i as u32, v);
        }
        assert_eq!(idx.len(), 6, "every real vector is indexed");
        assert_eq!(
            idx.search(&vecs[0], 1).first().map(|&(id, _)| id),
            Some(0),
            "and the re-inserted one is reachable"
        );
        for &id in idx.node_ids().iter() {
            assert_eq!(
                idx.back_refs_for_test(id),
                idx.scan_back_refs_for_test(id),
                "back_refs[{id}] disagrees with a full scan after a re-insertion"
            );
        }
    }

    /// A node removed while parked must stay removed: a later re-election of its
    /// dimension must not resurrect a vector the caller deleted.
    #[test]
    fn a_removed_node_does_not_return_from_the_parked_list() {
        const DIM: usize = 8;
        let vecs = make_unit_vecs(3, DIM, 0xDE1E_7ED0);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"parked-rm"));

        idx.insert(0, &vecs[0]);
        idx.insert(900, &[1.0, 2.0, 3.0]); // parks node 0
        idx.remove(0); // the caller deletes it while it is parked
        idx.insert(1, &vecs[1]); // re-elects 8 — node 0 must not come back
        assert_eq!(
            idx.node_ids(),
            [1].into_iter().collect(),
            "a removed node must not be revived by a re-election"
        );
        assert!(idx.can_answer(DIM));

        // An explicit insert supersedes a parked copy, so a re-election can never
        // revive a vector the node no longer has.
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"parked-stale"));
        idx.insert(0, &vecs[0]); // stride 8
        idx.insert(900, &[1.0, 2.0, 3.0]); // parks node 0's 8-D vector
        idx.insert(0, &[7.0, 8.0, 9.0]); // node 0 again, now 3-D
        assert_eq!(idx.len(), 2, "nodes 900 and 0, both at the 3-D stride");
        idx.remove(900); // back to one vector, so a re-election is possible
        idx.insert(2, &vecs[2]); // re-elects 8 and parks node 0's *3-D* vector
        assert_eq!(
            idx.node_ids(),
            [2].into_iter().collect(),
            "node 0's stale 8-D copy must not be revived — its vector is 3-D now"
        );
        assert!(
            idx.can_answer(8),
            "nothing of 8 dimensions is parked, so the index is complete at its \
             stride"
        );
    }

    /// A vector refused *after* the stride has settled leaves the index missing
    /// something it was offered, so it must stop claiming it can answer and let
    /// the caller scan. This is the property `hnsw_candidates`,
    /// `hnsw_search_dst` and `find_similar_vector` all hang their fallback on.
    #[test]
    fn an_index_that_refused_a_vector_will_not_claim_to_answer() {
        const DIM: usize = 64;
        let vecs = make_unit_vecs(30, DIM, 0xBADD_14E0);
        let mut idx = HnswIndex::new(crate::index::fnv1a_u64(b"refused"));
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u32, v);
        }
        assert!(idx.can_answer(DIM), "a clean index answers");

        hnsw_insert_count_reset();
        idx.insert(900, &[1.0, 2.0, 3.0]);
        assert_eq!(idx.len(), 30, "the stray must not be indexed");
        assert_eq!(hnsw_insert_count(), 0, "nor counted");
        assert!(
            !idx.can_answer(DIM),
            "an index that refused a vector must send the caller to its scan"
        );
        assert!(
            !idx.search(&vecs[3], 5).is_empty(),
            "`search` itself still answers — the fallback is the caller's \
             decision, taken on `can_answer`"
        );
    }

    /// The kernel sums in eight accumulators rather than one, which is a
    /// different summation order and therefore a different answer. This bounds
    /// how different: well inside the granularity at which candidate order can
    /// change, at every dimension including the awkward ones either side of the
    /// chunk width.
    #[test]
    fn the_dot_kernel_agrees_with_an_f64_reference() {
        for dim in [1usize, 7, 8, 15, 64, 1_536] {
            let vecs = make_unit_vecs(6, dim, 0x4047_0000 ^ dim as u64);
            for i in 0..vecs.len() {
                let a32: Vec<f32> = vecs[i].iter().map(|&x| x as f32).collect();
                let self_dot = dot_f32(&a32, &a32);
                assert!(
                    (self_dot as f64 - 1.0).abs() < 1e-5,
                    "dim {dim}: a unit vector dotted with itself is {self_dot}, not 1.0"
                );
                for j in 0..vecs.len() {
                    let b32: Vec<f32> = vecs[j].iter().map(|&x| x as f32).collect();
                    let reference: f64 =
                        vecs[i].iter().zip(vecs[j].iter()).map(|(a, b)| a * b).sum();
                    let got = dot_f32(&a32, &b32) as f64;
                    assert!(
                        (got - reference).abs() < 2e-5,
                        "dim {dim}, pair ({i},{j}): kernel {got} against reference \
                         {reference}"
                    );
                }
            }
        }
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

    /// Build a throwaway slab holding exactly `vecs`, normalized, one per slot,
    /// so a heuristic call can be made against known geometry. The prune reads
    /// the slab and the liveness map and nothing else.
    fn slab_of(vecs: &[Vec<f64>]) -> (VecSlab, Vec<u32>) {
        let mut slab = VecSlab::default();
        for (s, v) in vecs.iter().enumerate() {
            assert!(slab.put(s as u32, &l2_normalize(v).unwrap()));
        }
        let id_of = (0..vecs.len() as u32).collect();
        (slab, id_of)
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
        let (slab, id_of) = slab_of(&vecs);
        let base = as_f32(&base);
        let mut cands: Vec<(u32, f64)> = (0..6u32).map(|s| (s, dist_to(&slab, s, &base))).collect();
        sort_by_distance(&mut cands);
        assert_eq!(
            cands.iter().map(|&(s, _)| s).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5],
            "fixture: candidates must arrive in this nearest-first order"
        );

        let kept = HnswIndex::select_neighbors_first_rejection(&slab, &id_of, &cands, 2);
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
        let (slab, id_of) = slab_of(&vecs);
        let base = as_f32(&base);
        let mut cands: Vec<(u32, f64)> = (0..3u32).map(|s| (s, dist_to(&slab, s, &base))).collect();
        sort_by_distance(&mut cands);
        assert_eq!(
            cands.iter().map(|&(s, _)| s).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "fixture: nearest-first order"
        );

        let kept = HnswIndex::select_neighbors_first_rejection(&slab, &id_of, &cands, 2);
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
        let (slab, mut id_of) = slab_of(&vecs);
        id_of[1] = DEAD;
        let base = as_f32(&l2_normalize(&vecs[0]).unwrap());
        let mut cands: Vec<(u32, f64)> = (0..4u32).map(|s| (s, dist_to(&slab, s, &base))).collect();
        sort_by_distance(&mut cands);
        let kept = HnswIndex::select_neighbors_first_rejection(&slab, &id_of, &cands, 4);
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
            (mem.vector_floats * 4) as f64 / mem.live_nodes as f64,
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
        // 32 f32s per vector, exactly, or the fixture is not what it says.
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

    /// The blob-v2 on-disk shape, serialize side, so a test can write one.
    #[derive(Serialize)]
    struct V2Node {
        level: usize,
        vector: Vec<f64>,
        layers: Vec<Vec<u32>>,
    }

    #[derive(Serialize)]
    struct V2Index {
        base_seed: u64,
        slots: Vec<V2Node>,
        slot_of: BTreeMap<u32, u32>,
        id_of: Vec<u32>,
        free: Vec<u32>,
        entry_point: Option<u32>,
        max_level: usize,
    }

    #[derive(Serialize)]
    struct V2Blob {
        magic: [u8; 4],
        version: u16,
        index: V2Index,
    }

    /// A slot's vector back in `f64`, as the older shapes stored it. An `f32`
    /// widened to `f64` and narrowed again is bit-exact, so a round trip through
    /// either older blob loses nothing — which is what lets the upgrade tests
    /// compare `search` results exactly.
    fn vector_of(idx: &HnswIndex, slot: u32) -> Vec<f64> {
        idx.slab.get(slot).iter().map(|&x| x as f64).collect()
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
                        vector: vector_of(idx, s),
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

    /// Re-express a live index in the slot-keyed blob-v2 shape — 0.6.6 before the
    /// distance kernel, with an `f64` vector inside every node.
    fn as_v2_blob(idx: &HnswIndex) -> Vec<u8> {
        let slots: Vec<V2Node> = idx
            .slots
            .iter()
            .enumerate()
            .map(|(s, n)| V2Node {
                level: n.level,
                vector: vector_of(idx, s as u32),
                layers: n.layers.clone(),
            })
            .collect();
        bincode::serialize(&V2Blob {
            magic: HNSW_BLOB_MAGIC,
            version: 2,
            index: V2Index {
                base_seed: idx.base_seed,
                slots,
                slot_of: idx.slot_of.clone(),
                id_of: idx.id_of.clone(),
                free: idx.free.clone(),
                entry_point: idx.entry_point,
                max_level: idx.max_level,
            },
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

    /// This build's wrapper round-trips, and it is version 3.
    #[test]
    fn a_v3_blob_round_trips() {
        let (vecs, idx) = blob_fixture();
        let blob = encode_hnsw_blob(&idx).expect("encode");
        assert_eq!(
            &blob[..4],
            &HNSW_BLOB_MAGIC,
            "the blob must carry its magic"
        );
        assert_eq!(
            u16::from_le_bytes([blob[4], blob[5]]),
            3,
            "the slab shape is blob version 3"
        );

        hnsw_insert_count_reset();
        let loaded = decode_hnsw_blob(&blob).expect("a v3 blob must load");
        assert_eq!(hnsw_insert_count(), 0, "a load must not re-insert vectors");
        assert_matches(&loaded, &idx, &vecs[3]);
        assert_eq!(
            loaded.slab.dim, idx.slab.dim,
            "the slab's stride must survive the round trip"
        );
    }

    /// A blob written by 0.6.6 before the distance kernel — slot-keyed, with an
    /// `f64` vector per node — loads into the slab shape with no vector
    /// re-inserted and answers identically.
    #[test]
    fn a_v2_blob_upgrades_in_place() {
        let (vecs, idx) = blob_fixture();
        let blob = as_v2_blob(&idx);

        hnsw_insert_count_reset();
        let loaded = decode_hnsw_blob(&blob).expect("a v2 blob must still load");
        assert_eq!(
            hnsw_insert_count(),
            0,
            "up-converting a v2 blob must not re-insert a single vector"
        );
        assert_eq!(
            loaded.slab.dim, idx.slab.dim,
            "the slab's stride comes from the decoded vectors"
        );
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

    /// The downgrade this release actually ships into: a reader whose ceiling is
    /// version 2 — 0.6.6 up to Task 1 — meets a v3 blob and refuses it, so its
    /// caller keeps the `hnsw_tracked` full scan.
    ///
    /// The old reader is reconstructed here rather than asserted about: it read
    /// the wrapper against the v2 index shape and then refused any version above
    /// its own ceiling, so [`v2_era_decode`] is those two steps with
    /// [`HnswIndexV2`] — which this build still carries to *read* v2 blobs — in
    /// place of the live shape. A v2 blob proves the reconstruction works; a v3
    /// blob is then refused by it, on whichever of the two steps fires first.
    #[test]
    fn a_v2_reader_refuses_a_v3_blob_and_still_reads_a_v2_one() {
        /// `HNSW_BLOB_VERSION` as 0.6.6 shipped it before the distance kernel.
        const V2_CEILING: u16 = 2;

        /// The pre-3b decoder, in the two decisions it made: deserialize the
        /// wrapper against the shape it knew, then refuse an unknown version.
        fn v2_era_decode(blob: &[u8]) -> Result<usize, String> {
            match bincode::deserialize::<HnswBlobV2>(blob) {
                Ok(b) if b.magic == HNSW_BLOB_MAGIC => {
                    if b.version == 0 || b.version > V2_CEILING {
                        Err(format!("version {} is not readable", b.version))
                    } else {
                        Ok(b.index.slots.len())
                    }
                }
                Ok(b) => Err(format!("unrecognised magic {:?}", b.magic)),
                Err(e) => Err(format!("not a v2 blob ({e})")),
            }
        }

        let (_, idx) = blob_fixture();

        // Positive control: the reconstruction really does read a v2 blob, so its
        // refusal below is about the version and not about being broken.
        let v2 = as_v2_blob(&idx);
        assert_eq!(
            v2_era_decode(&v2),
            Ok(idx.slots.len()),
            "the reconstructed v2 reader must read a v2 blob"
        );

        // And it refuses this build's blob.
        let v3 = encode_hnsw_blob(&idx).expect("encode");
        assert_eq!(&v3[..4], &HNSW_BLOB_MAGIC, "same magic, new version");
        assert_eq!(u16::from_le_bytes([v3[4], v3[5]]), HNSW_BLOB_VERSION);
        let err = v2_era_decode(&v3).expect_err(
            "a build that reads up to v2 must refuse a v3 blob rather than \
             misread it — its caller then keeps the full scan",
        );
        eprintln!("a v2-era reader on a v3 blob: {err}");

        // The same gate in this build, for the version after this one: the
        // refusal names the version, which is what the caller logs before it
        // falls back.
        let mut future = v3.clone();
        future[4] = HNSW_BLOB_VERSION as u8 + 1;
        let err = decode_hnsw_blob(&future).expect_err("a future version must not be read");
        assert!(err.contains("version"), "{err}");
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

    /// The downgrade direction: a 0.6.5 binary meeting a v3 blob fails its
    /// `bincode::deserialize::<HnswIndex>` against the old shape rather than
    /// misreading it. `V1ReadIndex` is that old shape's read side.
    #[test]
    fn a_v3_blob_is_not_readable_as_a_v1_index() {
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

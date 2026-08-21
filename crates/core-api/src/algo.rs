//! Graph algorithms: PageRank, weakly-connected components, degree centrality.
//!
//! ## Dependency rule note
//!
//! This module lives in `core-api` (not `core-query`) because it must read the
//! *unified topology* — manual edges from `Topology` plus derived edges written
//! there by the rule engine via `GraphMut`. `core-query` has no dependency on
//! `core-rules` and therefore cannot see derived provenance.  `GraphDb` fields
//! are private; the algorithms are pure functions called from `GraphDb` methods
//! that pass in the already-unified `&Topology` (which already contains both
//! manual and rule-derived edges).
//!
//! ## When to use views vs `degree_centrality`
//!
//! `degree_centrality` is a **one-shot compute**: call it, get a snapshot sorted
//! by degree, done.  It does not persist anywhere and is not maintained as
//! properties change.  Use it for offline analysis, ranking a batch, or feeding
//! `write_scores` once.
//!
//! A **Degree materialized view** (`ViewDef { kind: AggFn::Degree, … }`) is
//! *maintained incrementally*: every `insert_edge` / `delete_edge` /
//! `insert_node` re-computes just the affected node's count and stores it as a
//! live property.  Use it when you need the degree of individual nodes at query
//! time with zero latency (e.g. `MATCH (n) WHERE n.out_degree > 5 RETURN n`).
//!
//! Rule of thumb: if you need the top-K by degree once → `degree_centrality`;
//! if you need the degree of every node available in every Cypher query →
//! create a Degree view.

use core_query::Dir;
use core_storage::{Direction, IdMap, Interner, Topology};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Collect the dense list of live node ids and their string keys.
///
/// Returns `(ids, keys)` where `ids[i]` is the internal u32 id for `keys[i]`.
/// Tombstoned slots are skipped.  Stable order: ascending internal id.
fn live_nodes(idmap: &IdMap, labels: &[u32]) -> (Vec<u32>, Vec<String>) {
    let n = idmap.len() as u32;
    let mut ids = Vec::new();
    let mut keys = Vec::new();
    for id in 0..n {
        let Some(key) = idmap.key_of(id) else { continue };
        let Some(&sym) = labels.get(id as usize) else { continue };
        if sym == u32::MAX {
            continue; // tombstoned
        }
        ids.push(id);
        keys.push(key.to_string());
    }
    (ids, keys)
}

/// Resolve an optional edge-type name to its interned symbol.
///
/// Returns `None` if `edge_type` is `Some(name)` that is not interned (meaning
/// no edges of that type exist).  Returns `Some(None)` when `edge_type` is
/// `None` (all types).
fn resolve_etype(
    syms: &Interner,
    edge_type: Option<&str>,
) -> Option<Option<u32>> {
    match edge_type {
        None => Some(None), // all etypes
        Some(name) => {
            let sym = syms.get(name)?; // not interned → no such edges
            Some(Some(sym))
        }
    }
}

/// Iterate over etypes in the topology, optionally filtered to a single etype.
fn etypes_filtered(topo: &Topology, filter: Option<u32>) -> Vec<u32> {
    match filter {
        Some(sym) => {
            // Only include if the etype actually exists.
            let all: Vec<u32> = topo.etypes().collect();
            if all.contains(&sym) { vec![sym] } else { vec![] }
        }
        None => topo.etypes().collect(),
    }
}

// ---------------------------------------------------------------------------
// PageRank
// ---------------------------------------------------------------------------

/// Configuration for [`GraphDb::pagerank`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PageRankConfig {
    /// Damping factor (probability of following an edge, not teleporting).
    /// Default 0.85.
    pub damping: f64,
    /// Maximum number of power-iteration steps. Default 50.
    pub max_iters: u32,
    /// Convergence tolerance (L1 norm over all nodes). Default 1e-6.
    pub tol: f64,
    /// Restrict edges to this type. `None` uses all edge types (unified topology).
    pub edge_type: Option<String>,
    /// Edge direction to follow. `Dir::Out` follows out-edges (standard web
    /// PageRank); `Dir::In` follows in-edges (authority scores); `Dir::Both`
    /// treats all edges as undirected.
    pub direction: AlgoDir,
    /// Wall-clock budget (milliseconds) for the HTTP server endpoint.
    /// `0` means no budget (run to convergence or `max_iters`).
    pub budget_ms: u64,
}

impl Default for PageRankConfig {
    fn default() -> Self {
        Self {
            damping: 0.85,
            max_iters: 50,
            tol: 1e-6,
            edge_type: None,
            direction: AlgoDir::Out,
            budget_ms: 5_000,
        }
    }
}

/// Result of [`GraphDb::pagerank`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageRankReport {
    /// Node keys and their PageRank scores.  Sorted: score descending, key
    /// ascending on ties (deterministic).
    pub scores: Vec<(String, f64)>,
    /// `true` if the algorithm converged before `max_iters` and before any time
    /// budget fired.  `false` means scores are still valid but partial — more
    /// iterations would refine them.
    pub converged: bool,
}

/// Direction semantics for algo methods (mirrors `Dir` but serializable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlgoDir {
    /// Follow outgoing edges only (standard directed PageRank / out-degree).
    Out,
    /// Follow incoming edges only (in-degree / authority score).
    In,
    /// Treat edges as undirected: Out ∪ In.
    Both,
}

impl From<Dir> for AlgoDir {
    fn from(d: Dir) -> Self {
        match d {
            Dir::Out => AlgoDir::Out,
            Dir::In => AlgoDir::In,
            Dir::Both => AlgoDir::Both,
        }
    }
}

/// Run PageRank on the unified topology.
///
/// Returns a [`PageRankReport`] with scores sorted descending (ties: key asc).
pub(crate) fn pagerank(
    topo: &Topology,
    idmap: &IdMap,
    syms: &Interner,
    labels: &[u32],
    config: &PageRankConfig,
) -> PageRankReport {
    let deadline = if config.budget_ms > 0 {
        Some(Instant::now() + Duration::from_millis(config.budget_ms))
    } else {
        None
    };

    let (node_ids, node_keys) = live_nodes(idmap, labels);
    let n = node_ids.len();

    if n == 0 {
        return PageRankReport { scores: Vec::new(), converged: true };
    }

    // Map internal id → compact index for fast array access.
    let max_id = topo.etypes().count(); // just an upper bound check hint
    let _ = max_id;
    let mut id_to_idx: BTreeMap<u32, usize> = BTreeMap::new();
    for (i, &id) in node_ids.iter().enumerate() {
        id_to_idx.insert(id, i);
    }

    // Resolve etype filter.
    let etype_filter = match resolve_etype(syms, config.edge_type.as_deref()) {
        None => {
            // Edge type specified but not in graph → no edges, PR is uniform.
            let score = 1.0 / n as f64;
            let mut scores: Vec<(String, f64)> =
                node_keys.iter().map(|k| (k.clone(), score)).collect();
            scores.sort_by(|(ka, sa), (kb, sb)| {
                sb.partial_cmp(sa).unwrap_or(std::cmp::Ordering::Equal).then(ka.cmp(kb))
            });
            return PageRankReport { scores, converged: true };
        }
        Some(f) => f,
    };

    let etypes = etypes_filtered(topo, etype_filter);

    // Build adjacency list (compact index): for each compact node,
    // which compact nodes does it "send" rank to (based on direction)?
    // send_to[i] = sorted list of compact indices that node i sends rank to.
    let mut send_to: Vec<Vec<usize>> = vec![Vec::new(); n];

    for &et in &etypes {
        for (i, &id) in node_ids.iter().enumerate() {
            match config.direction {
                AlgoDir::Out => {
                    // Standard: node i sends to its out-neighbors.
                    for &nbr in topo.neighbors(et, Direction::Out, id) {
                        if let Some(&j) = id_to_idx.get(&nbr) {
                            if !send_to[i].contains(&j) {
                                send_to[i].push(j);
                            }
                        }
                    }
                }
                AlgoDir::In => {
                    // Authority: node i sends to its in-neighbors (reversed).
                    for &nbr in topo.neighbors(et, Direction::In, id) {
                        if let Some(&j) = id_to_idx.get(&nbr) {
                            if !send_to[i].contains(&j) {
                                send_to[i].push(j);
                            }
                        }
                    }
                }
                AlgoDir::Both => {
                    // Undirected: union of out and in.
                    for dir in [Direction::Out, Direction::In] {
                        for &nbr in topo.neighbors(et, dir, id) {
                            if let Some(&j) = id_to_idx.get(&nbr) {
                                if !send_to[i].contains(&j) {
                                    send_to[i].push(j);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Build receive_from[j] = list of (i, 1/out_degree(i)) that send to j.
    // Also track dangling nodes (send_to.is_empty()).
    let mut receive_from: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut dangling: Vec<usize> = Vec::new();

    for (i, send) in send_to.iter().enumerate() {
        let out_deg = send.len();
        if out_deg == 0 {
            dangling.push(i);
        } else {
            let w = 1.0 / out_deg as f64;
            for &j in send {
                receive_from[j].push((i, w));
            }
        }
    }

    // Power iteration.
    let nf = n as f64;
    let d = config.damping;
    let teleport = (1.0 - d) / nf;
    let mut pr: Vec<f64> = vec![1.0 / nf; n];
    let mut converged = false;

    for _iter in 0..config.max_iters {
        // Check time budget between iterations.
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                break;
            }
        }

        // Sum PR leaked by dangling nodes → distribute uniformly.
        let dangling_sum: f64 = dangling.iter().map(|&i| pr[i]).sum::<f64>() * d / nf;

        let mut new_pr = vec![teleport + dangling_sum; n];
        for j in 0..n {
            let received: f64 = receive_from[j].iter().map(|&(i, w)| pr[i] * w).sum();
            new_pr[j] += d * received;
        }

        // Check convergence: L1 norm.
        let delta: f64 = pr.iter().zip(new_pr.iter()).map(|(a, b)| (a - b).abs()).sum();
        pr = new_pr;

        if delta < config.tol {
            converged = true;
            break;
        }
    }

    // Sort: score desc, key asc on ties.
    let mut scores: Vec<(String, f64)> = node_keys
        .into_iter()
        .zip(pr)
        .collect();
    scores.sort_by(|(ka, sa), (kb, sb)| {
        sb.partial_cmp(sa).unwrap_or(std::cmp::Ordering::Equal).then(ka.cmp(kb))
    });

    PageRankReport { scores, converged }
}

// ---------------------------------------------------------------------------
// Weakly-connected components (WCC)
// ---------------------------------------------------------------------------

/// Configuration for [`GraphDb::connected_components`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WccConfig {
    /// Restrict edges to this type. `None` uses all edge types.
    pub edge_type: Option<String>,
    /// Wall-clock budget (milliseconds) for the HTTP server endpoint.
    pub budget_ms: u64,
}

impl Default for WccConfig {
    fn default() -> Self {
        Self { edge_type: None, budget_ms: 5_000 }
    }
}

/// Result of [`GraphDb::connected_components`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WccReport {
    /// Each live node and the key of the smallest member of its component
    /// (deterministic component identifier).  Sorted by (component_id, key).
    pub components: Vec<(String, String)>,
    /// `true` if the time budget fired before all nodes were processed.
    pub truncated: bool,
}

/// Union-Find with path compression and union by rank.
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]]; // path halving
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}

/// Run weakly-connected components on the unified topology (undirected).
///
/// Component ID is the smallest member key in each component (deterministic).
pub(crate) fn wcc(
    topo: &Topology,
    idmap: &IdMap,
    syms: &Interner,
    labels: &[u32],
    config: &WccConfig,
) -> WccReport {
    let deadline = if config.budget_ms > 0 {
        Some(Instant::now() + Duration::from_millis(config.budget_ms))
    } else {
        None
    };

    let (node_ids, node_keys) = live_nodes(idmap, labels);
    let n = node_ids.len();

    if n == 0 {
        return WccReport { components: Vec::new(), truncated: false };
    }

    // Map internal id → compact index.
    let mut id_to_idx: BTreeMap<u32, usize> = BTreeMap::new();
    for (i, &id) in node_ids.iter().enumerate() {
        id_to_idx.insert(id, i);
    }

    // Resolve etype filter.
    let etype_filter = match resolve_etype(syms, config.edge_type.as_deref()) {
        None => {
            // No edges of this type → every node is its own component.
            let mut components: Vec<(String, String)> =
                node_keys.iter().map(|k| (k.clone(), k.clone())).collect();
            components.sort();
            return WccReport { components, truncated: false };
        }
        Some(f) => f,
    };

    let etypes = etypes_filtered(topo, etype_filter);

    let mut uf = UnionFind::new(n);
    let mut truncated = false;

    // Union all edges (both directions — WCC treats graph as undirected).
    'outer: for &et in &etypes {
        for (i, &id) in node_ids.iter().enumerate() {
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    truncated = true;
                    break 'outer;
                }
            }
            // Out-edges: union i with each out-neighbor.
            for &nbr in topo.neighbors(et, Direction::Out, id) {
                if let Some(&j) = id_to_idx.get(&nbr) {
                    uf.union(i, j);
                }
            }
            // In-edges handled by the mirror out-edge from the other side,
            // but we also cover them here for safety (e.g., self-loops, or
            // nodes with only in-edges for a filtered etype).
            for &nbr in topo.neighbors(et, Direction::In, id) {
                if let Some(&j) = id_to_idx.get(&nbr) {
                    uf.union(i, j);
                }
            }
        }
    }

    // Determine component representative: smallest key per root.
    let mut root_min_key: BTreeMap<usize, &str> = BTreeMap::new();
    for (i, key_str) in node_keys.iter().enumerate() {
        let root = uf.find(i);
        let key = key_str.as_str();
        let entry = root_min_key.entry(root).or_insert(key);
        if key < *entry {
            *entry = key;
        }
    }

    let mut components: Vec<(String, String)> = node_keys
        .iter()
        .enumerate()
        .map(|(i, key_str)| {
            let root = uf.find(i);
            let comp_id = root_min_key[&root].to_string();
            (key_str.clone(), comp_id)
        })
        .collect();
    components.sort_by(|(ka, ca), (kb, cb)| ca.cmp(cb).then(ka.cmp(kb)));

    WccReport { components, truncated }
}

// ---------------------------------------------------------------------------
// Degree centrality
// ---------------------------------------------------------------------------

/// Configuration for [`GraphDb::degree_centrality`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DegreeConfig {
    /// Restrict edges to this type. `None` counts all edge types.
    pub edge_type: Option<String>,
    /// Which edges to count per node.
    pub direction: AlgoDir,
    /// Wall-clock budget (milliseconds) for the HTTP server endpoint.
    pub budget_ms: u64,
}

impl Default for DegreeConfig {
    fn default() -> Self {
        Self {
            edge_type: None,
            direction: AlgoDir::Both,
            budget_ms: 5_000,
        }
    }
}

/// Result of [`GraphDb::degree_centrality`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegreeReport {
    /// Node keys and their degree.  Sorted: degree descending, key ascending on ties.
    pub scores: Vec<(String, u64)>,
    /// `true` if the time budget fired before all nodes were processed.
    pub truncated: bool,
}

/// Compute degree centrality for all live nodes.
pub(crate) fn degree_centrality(
    topo: &Topology,
    idmap: &IdMap,
    syms: &Interner,
    labels: &[u32],
    config: &DegreeConfig,
) -> DegreeReport {
    let deadline = if config.budget_ms > 0 {
        Some(Instant::now() + Duration::from_millis(config.budget_ms))
    } else {
        None
    };

    let (node_ids, node_keys) = live_nodes(idmap, labels);
    let n = node_ids.len();

    if n == 0 {
        return DegreeReport { scores: Vec::new(), truncated: false };
    }

    // Resolve etype filter.
    let etype_filter = match resolve_etype(syms, config.edge_type.as_deref()) {
        None => {
            // No edges of this type → all degrees are 0.
            let scores = node_keys.iter().map(|k| (k.clone(), 0u64)).collect();
            return DegreeReport { scores, truncated: false };
        }
        Some(f) => f,
    };

    let etypes = etypes_filtered(topo, etype_filter);
    let mut degrees: Vec<u64> = vec![0u64; n];
    let mut truncated = false;

    for (i, &id) in node_ids.iter().enumerate() {
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                truncated = true;
                break;
            }
        }
        for &et in &etypes {
            match config.direction {
                AlgoDir::Out => {
                    degrees[i] += topo.neighbors(et, Direction::Out, id).len() as u64;
                }
                AlgoDir::In => {
                    degrees[i] += topo.neighbors(et, Direction::In, id).len() as u64;
                }
                AlgoDir::Both => {
                    degrees[i] += topo.neighbors(et, Direction::Out, id).len() as u64;
                    degrees[i] += topo.neighbors(et, Direction::In, id).len() as u64;
                }
            }
        }
    }

    let mut scores: Vec<(String, u64)> = node_keys
        .into_iter()
        .zip(degrees)
        .collect();
    scores.sort_by(|(ka, da), (kb, db)| db.cmp(da).then(ka.cmp(kb)));

    DegreeReport { scores, truncated }
}

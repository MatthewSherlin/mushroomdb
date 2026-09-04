//! Graph algorithms: PageRank, weakly-connected components, degree centrality.
//!
//! ## Dependency rule note
//!
//! This module lives in `core-api` (not `core-query`) because it must read the
//! *unified topology* — manual edges plus derived edges written by the rule
//! engine via `GraphMut`. `core-query` has no dependency on `core-rules` and
//! therefore cannot see derived provenance.  `GraphDb` fields are private; the
//! algorithms are pure functions called from `GraphDb` methods that pass in a
//! [`TopologyView`] over the in-memory overlay **and** the mmap V8 base. Reading
//! the view (not the bare overlay `Topology`) is required: after a snapshot
//! reopen the derived edges live in the base, and using the overlay alone would
//! report zero degree/rank for every node.
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
use core_storage::v8::seam::TopologyView;
use core_storage::{Direction, EdgePropsView, IdMap, Interner, Value};
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
        let Some(key) = idmap.key_of(id) else {
            continue;
        };
        let Some(&sym) = labels.get(id as usize) else {
            continue;
        };
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
fn resolve_etype(syms: &Interner, edge_type: Option<&str>) -> Option<Option<u32>> {
    match edge_type {
        None => Some(None), // all etypes
        Some(name) => {
            let sym = syms.get(name)?; // not interned → no such edges
            Some(Some(sym))
        }
    }
}

/// Iterate over etypes in the topology, optionally filtered to a single etype.
fn etypes_filtered(topo: &TopologyView, filter: Option<u32>) -> Vec<u32> {
    match filter {
        Some(sym) => {
            // Only include if the etype actually exists.
            let all: Vec<u32> = topo.etypes().collect();
            if all.contains(&sym) {
                vec![sym]
            } else {
                vec![]
            }
        }
        None => topo.etypes().collect(),
    }
}

/// Resolve a list of edge-type names to their interned symbols, in the given
/// order, de-duplicated. Unresolved names (not interned — no such edges
/// exist) are silently skipped. An empty `names` means "all edge types".
fn resolve_etypes_multi(syms: &Interner, topo: &TopologyView, names: &[String]) -> Vec<u32> {
    if names.is_empty() {
        return topo.etypes().collect();
    }
    let mut out = Vec::new();
    for name in names {
        if let Some(sym) = syms.get(name) {
            if !out.contains(&sym) {
                out.push(sym);
            }
        }
    }
    out
}

/// Like [`live_nodes`] but optionally restricted to nodes carrying `label`.
/// `None` includes every live node; `Some(name)` not interned yields an empty
/// result (no such nodes exist).
fn live_nodes_for_label(
    idmap: &IdMap,
    syms: &Interner,
    labels: &[u32],
    label: Option<&str>,
) -> (Vec<u32>, Vec<String>) {
    let want = match label {
        None => None,
        Some(name) => match syms.get(name) {
            Some(sym) => Some(sym),
            None => return (Vec::new(), Vec::new()),
        },
    };
    let n = idmap.len() as u32;
    let mut ids = Vec::new();
    let mut keys = Vec::new();
    for id in 0..n {
        let Some(key) = idmap.key_of(id) else {
            continue;
        };
        let Some(&sym) = labels.get(id as usize) else {
            continue;
        };
        if sym == u32::MAX {
            continue; // tombstoned
        }
        if let Some(want_sym) = want {
            if sym != want_sym {
                continue;
            }
        }
        ids.push(id);
        keys.push(key.to_string());
    }
    (ids, keys)
}

/// Resolve the weight of edge `(etype, src, dst)`.
///
/// `weight_prop`: read this numeric edge property; missing or non-numeric
/// values fall back to `1.0`. `None` treats every edge as weight `1.0`.
/// `min_weight`: drop the edge (return `None`) when its resolved weight is
/// below this threshold — applied regardless of whether `weight_prop` is set.
fn edge_weight(
    edge_props: &EdgePropsView,
    etype: u32,
    src: u32,
    dst: u32,
    weight_prop: Option<&str>,
    min_weight: Option<f64>,
) -> Option<f64> {
    let w = match weight_prop {
        None => 1.0,
        Some(prop) => match edge_props.get(etype, src, dst, prop) {
            Some(Value::Float(f)) => f,
            Some(Value::Int(i)) => i as f64,
            _ => 1.0,
        },
    };
    match min_weight {
        Some(min) if w < min => None,
        _ => Some(w),
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
    /// Read this edge property as the edge weight; missing or non-numeric
    /// values fall back to `1.0`. `None` treats every edge as weight `1.0`
    /// (mass distributes uniformly across out-edges, as before).
    pub weight_prop: Option<String>,
    /// Drop edges whose resolved weight is below this threshold before the
    /// algorithm runs. Applied whether or not `weight_prop` is set.
    pub min_weight: Option<f64>,
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
            weight_prop: None,
            min_weight: None,
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
    topo: &TopologyView,
    idmap: &IdMap,
    syms: &Interner,
    labels: &[u32],
    edge_props: &EdgePropsView,
    config: &PageRankConfig,
) -> PageRankReport {
    let weighted = config.weight_prop.is_some() || config.min_weight.is_some();
    let deadline = if config.budget_ms > 0 {
        Some(Instant::now() + Duration::from_millis(config.budget_ms))
    } else {
        None
    };

    let (node_ids, node_keys) = live_nodes(idmap, labels);
    let n = node_ids.len();

    if n == 0 {
        return PageRankReport {
            scores: Vec::new(),
            converged: true,
        };
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
                sb.partial_cmp(sa)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(ka.cmp(kb))
            });
            return PageRankReport {
                scores,
                converged: true,
            };
        }
        Some(f) => f,
    };

    let etypes = etypes_filtered(topo, etype_filter);

    // Build adjacency list (compact index): for each compact node, which
    // compact nodes does it "send" rank to (based on direction), and with
    // what weight.  Unweighted mode dedups parallel edges to the same
    // neighbor (matches pre-weight behavior exactly); weighted mode sums the
    // resolved weight of every qualifying edge instance (accumulating across
    // parallel edges / multiple edge types).
    let mut send_to: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];

    for &et in &etypes {
        for (i, &id) in node_ids.iter().enumerate() {
            let dirs: &[Direction] = match config.direction {
                AlgoDir::Out => &[Direction::Out],
                AlgoDir::In => &[Direction::In],
                AlgoDir::Both => &[Direction::Out, Direction::In],
            };
            for &dir in dirs {
                for &nbr in topo.neighbors(et, dir, id).as_ref() {
                    let Some(&j) = id_to_idx.get(&nbr) else {
                        continue;
                    };
                    if weighted {
                        let Some(w) = edge_weight(
                            edge_props,
                            et,
                            id,
                            nbr,
                            config.weight_prop.as_deref(),
                            config.min_weight,
                        ) else {
                            continue; // filtered by min_weight
                        };
                        if let Some(entry) = send_to[i].iter_mut().find(|(k, _)| *k == j) {
                            entry.1 += w;
                        } else {
                            send_to[i].push((j, w));
                        }
                    } else if !send_to[i].iter().any(|(k, _)| *k == j) {
                        send_to[i].push((j, 1.0));
                    }
                }
            }
        }
    }

    // Build receive_from[j] = list of (i, share) that send to j, where share
    // is i's outgoing weight to j normalized by i's total outgoing weight.
    // Also track dangling nodes (no outgoing weight).
    let mut receive_from: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut dangling: Vec<usize> = Vec::new();

    for (i, send) in send_to.iter().enumerate() {
        let out_weight: f64 = send.iter().map(|(_, w)| w).sum();
        if send.is_empty() || out_weight <= 0.0 {
            dangling.push(i);
        } else {
            for &(j, w) in send {
                receive_from[j].push((i, w / out_weight));
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
        let delta: f64 = pr
            .iter()
            .zip(new_pr.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        pr = new_pr;

        if delta < config.tol {
            converged = true;
            break;
        }
    }

    // Sort: score desc, key asc on ties.
    let mut scores: Vec<(String, f64)> = node_keys.into_iter().zip(pr).collect();
    scores.sort_by(|(ka, sa), (kb, sb)| {
        sb.partial_cmp(sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(ka.cmp(kb))
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
    /// Read this edge property as the edge weight; missing or non-numeric
    /// values fall back to `1.0`. Only used together with `min_weight` — WCC
    /// itself is unweighted, but a weighted edge can still be filtered out.
    pub weight_prop: Option<String>,
    /// Drop edges whose resolved weight is below this threshold before the
    /// algorithm runs. Applied whether or not `weight_prop` is set.
    pub min_weight: Option<f64>,
}

impl Default for WccConfig {
    fn default() -> Self {
        Self {
            edge_type: None,
            budget_ms: 5_000,
            weight_prop: None,
            min_weight: None,
        }
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
    topo: &TopologyView,
    idmap: &IdMap,
    syms: &Interner,
    labels: &[u32],
    edge_props: &EdgePropsView,
    config: &WccConfig,
) -> WccReport {
    let weighted = config.weight_prop.is_some() || config.min_weight.is_some();
    let deadline = if config.budget_ms > 0 {
        Some(Instant::now() + Duration::from_millis(config.budget_ms))
    } else {
        None
    };

    let (node_ids, node_keys) = live_nodes(idmap, labels);
    let n = node_ids.len();

    if n == 0 {
        return WccReport {
            components: Vec::new(),
            truncated: false,
        };
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
            return WccReport {
                components,
                truncated: false,
            };
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
            for &nbr in topo.neighbors(et, Direction::Out, id).as_ref() {
                if let Some(&j) = id_to_idx.get(&nbr) {
                    if weighted
                        && edge_weight(
                            edge_props,
                            et,
                            id,
                            nbr,
                            config.weight_prop.as_deref(),
                            config.min_weight,
                        )
                        .is_none()
                    {
                        continue; // filtered by min_weight
                    }
                    uf.union(i, j);
                }
            }
            // In-edges handled by the mirror out-edge from the other side,
            // but we also cover them here for safety (e.g., self-loops, or
            // nodes with only in-edges for a filtered etype).
            for &nbr in topo.neighbors(et, Direction::In, id).as_ref() {
                if let Some(&j) = id_to_idx.get(&nbr) {
                    if weighted
                        && edge_weight(
                            edge_props,
                            et,
                            nbr,
                            id,
                            config.weight_prop.as_deref(),
                            config.min_weight,
                        )
                        .is_none()
                    {
                        continue; // filtered by min_weight
                    }
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

    WccReport {
        components,
        truncated,
    }
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
    /// Read this edge property as the edge weight; missing or non-numeric
    /// values fall back to `1.0`. Degree stays an unweighted count of the
    /// edges that survive `min_weight` filtering — this does not weight the
    /// count itself.
    pub weight_prop: Option<String>,
    /// Drop edges whose resolved weight is below this threshold before the
    /// algorithm runs. Applied whether or not `weight_prop` is set.
    pub min_weight: Option<f64>,
}

impl Default for DegreeConfig {
    fn default() -> Self {
        Self {
            edge_type: None,
            direction: AlgoDir::Both,
            budget_ms: 5_000,
            weight_prop: None,
            min_weight: None,
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
    topo: &TopologyView,
    idmap: &IdMap,
    syms: &Interner,
    labels: &[u32],
    edge_props: &EdgePropsView,
    config: &DegreeConfig,
) -> DegreeReport {
    let weighted = config.weight_prop.is_some() || config.min_weight.is_some();
    let deadline = if config.budget_ms > 0 {
        Some(Instant::now() + Duration::from_millis(config.budget_ms))
    } else {
        None
    };

    let (node_ids, node_keys) = live_nodes(idmap, labels);
    let n = node_ids.len();

    if n == 0 {
        return DegreeReport {
            scores: Vec::new(),
            truncated: false,
        };
    }

    // Resolve etype filter.
    let etype_filter = match resolve_etype(syms, config.edge_type.as_deref()) {
        None => {
            // No edges of this type → all degrees are 0.
            let scores = node_keys.iter().map(|k| (k.clone(), 0u64)).collect();
            return DegreeReport {
                scores,
                truncated: false,
            };
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
            let dirs: &[Direction] = match config.direction {
                AlgoDir::Out => &[Direction::Out],
                AlgoDir::In => &[Direction::In],
                AlgoDir::Both => &[Direction::Out, Direction::In],
            };
            for &dir in dirs {
                if !weighted {
                    degrees[i] += topo.neighbors(et, dir, id).len() as u64;
                    continue;
                }
                for &nbr in topo.neighbors(et, dir, id).as_ref() {
                    let (src, dst) = match dir {
                        Direction::Out => (id, nbr),
                        Direction::In => (nbr, id),
                    };
                    if edge_weight(
                        edge_props,
                        et,
                        src,
                        dst,
                        config.weight_prop.as_deref(),
                        config.min_weight,
                    )
                    .is_some()
                    {
                        degrees[i] += 1;
                    }
                }
            }
        }
    }

    let mut scores: Vec<(String, u64)> = node_keys.into_iter().zip(degrees).collect();
    scores.sort_by(|(ka, da), (kb, db)| db.cmp(da).then(ka.cmp(kb)));

    DegreeReport { scores, truncated }
}

// ---------------------------------------------------------------------------
// Louvain community detection
// ---------------------------------------------------------------------------

/// Weighted undirected adjacency list: `adj[i]` is `(neighbor, weight)` pairs
/// for compact node index `i`. Never contains a self-entry (`i == neighbor`)
/// — internal/self weight is tracked separately (see `local_moving`,
/// `aggregate`).
type WeightedAdj = Vec<Vec<(usize, f64)>>;

/// The next (coarser) level's graph, built by [`aggregate`].
struct AggregatedLevel {
    /// `renumbered[i]` is node `i`'s (at the level just aggregated) new,
    /// compact `0..n` community id — callers compose this directly into
    /// their own node→community mapping.
    renumbered: Vec<usize>,
    n: usize,
    adj: WeightedAdj,
    self_weight: Vec<f64>,
}

/// Configuration for [`GraphDb::communities`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LouvainConfig {
    /// Restrict to the union of these edge types. Empty means all edge types
    /// (manual + rule-derived, via the unified topology).
    pub edge_types: Vec<String>,
    /// Read this edge property as the edge weight; missing or non-numeric
    /// values fall back to `1.0`. `None` treats every edge as weight `1.0`.
    pub weight_prop: Option<String>,
    /// Drop edges whose resolved weight is below this threshold before the
    /// algorithm runs. Applied whether or not `weight_prop` is set.
    pub min_weight: Option<f64>,
    /// Modularity resolution parameter (`γ`). Default `1.0`; values above 1
    /// favor more, smaller communities; below 1 favor fewer, larger ones.
    pub resolution: f64,
    /// Maximum number of local-moving + aggregation passes.
    pub max_passes: u32,
    /// Maximum local-moving sweeps within a single pass.
    pub max_sweeps: u32,
    /// Wall-clock budget (milliseconds), checked once per sweep. `0` means no
    /// budget (run to convergence or `max_passes`/`max_sweeps`).
    pub budget_ms: u64,
    /// Restrict membership to nodes carrying this label. Edges touching a
    /// node outside the label set are ignored.
    pub node_label: Option<String>,
}

impl Default for LouvainConfig {
    fn default() -> Self {
        Self {
            edge_types: Vec::new(),
            weight_prop: None,
            min_weight: None,
            resolution: 1.0,
            max_passes: 10,
            max_sweeps: 20,
            budget_ms: 5_000,
            node_label: None,
        }
    }
}

/// One detected community.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Community {
    /// 0-based id assigned by output order (position in
    /// [`CommunityReport::communities`]) — not stable across different runs
    /// with a different partition shape.
    pub id: u32,
    /// Member node keys, sorted ascending.
    pub members: Vec<String>,
    /// Total weight of edges with both endpoints inside this community (each
    /// undirected edge counted once), computed from the original edges after
    /// `weight_prop`/`min_weight` filtering — not from the aggregated
    /// intermediate levels the algorithm builds internally.
    pub internal_weight: f64,
    /// `internal_weight / (internal_weight + weight of edges leaving the
    /// community)`. `1.0` for a community with no incident edges at all
    /// (trivially cohesive).
    pub cohesion: f64,
}

/// Result of [`GraphDb::communities`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommunityReport {
    /// Sorted: size descending, then smallest member key ascending on ties.
    pub communities: Vec<Community>,
    /// Modularity of the final partition (resolution-adjusted).
    pub modularity: f64,
    /// `true` if the time budget fired before local moving converged.
    pub truncated: bool,
}

/// One level's local-moving phase: greedily reassigns each node to the
/// neighboring community (or itself) that maximizes modularity gain, in
/// sorted node order, sweeping until stable or `max_sweeps`.
///
/// Returns `(community_of, hit_budget)`. `community_of[i]` is a community id
/// drawn from `0..n` (not necessarily contiguous). `hit_budget` is `true`
/// when the deadline fired before local moving converged — the returned
/// assignment is then whatever the sweeps completed before the deadline.
fn local_moving(
    n: usize,
    adj: &WeightedAdj,
    self_weight: &[f64],
    resolution: f64,
    max_sweeps: u32,
    deadline: Option<Instant>,
) -> (Vec<usize>, bool) {
    let k: Vec<f64> = (0..n)
        .map(|i| adj[i].iter().map(|&(_, w)| w).sum::<f64>() + 2.0 * self_weight[i])
        .collect();
    let m: f64 = k.iter().sum::<f64>() / 2.0;
    let mut community_of: Vec<usize> = (0..n).collect();
    if m <= 0.0 {
        return (community_of, false);
    }
    let mut tot: Vec<f64> = k.clone();

    for _sweep in 0..max_sweeps {
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                return (community_of, true);
            }
        }
        let mut improved = false;
        for i in 0..n {
            let ci = community_of[i];
            tot[ci] -= k[i];

            // Weight from i to each neighboring community, keyed by
            // community id ascending (BTreeMap) so tie-breaking below is
            // deterministic regardless of adjacency iteration order.
            let mut neighbor_weights: BTreeMap<usize, f64> = BTreeMap::new();
            for &(j, w) in &adj[i] {
                if j == i {
                    continue; // no self entries stored in adj; defensive
                }
                *neighbor_weights.entry(community_of[j]).or_insert(0.0) += w;
            }

            let gain = |c: usize, w_in: f64| -> f64 {
                w_in / m - resolution * tot[c] * k[i] / (2.0 * m * m)
            };

            let mut best_c = ci;
            let mut best_gain = gain(ci, neighbor_weights.get(&ci).copied().unwrap_or(0.0));
            for (&c, &w_in) in &neighbor_weights {
                if c == ci {
                    continue;
                }
                let g = gain(c, w_in);
                if g > best_gain + 1e-12 {
                    best_gain = g;
                    best_c = c;
                }
            }

            tot[best_c] += k[i];
            if best_c != ci {
                community_of[i] = best_c;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }

    (community_of, false)
}

/// Build the next level's aggregated (super-node) graph from a completed
/// local-moving assignment. Each distinct community becomes one super-node;
/// edges within a community fold into its self-weight, edges crossing
/// communities sum into the new adjacency.
///
/// Returns `None` when no coarsening happened (every node kept its own
/// singleton community) — a local optimum where further aggregation would
/// have no effect.
fn aggregate(
    n: usize,
    adj: &WeightedAdj,
    self_weight: &[f64],
    community_of: &[usize],
) -> Option<AggregatedLevel> {
    let mut remap: BTreeMap<usize, usize> = BTreeMap::new();
    let mut next_id = 0usize;
    let mut renumbered: Vec<usize> = vec![0; n];
    for (i, item) in renumbered.iter_mut().enumerate() {
        let c = community_of[i];
        let idx = *remap.entry(c).or_insert_with(|| {
            let id = next_id;
            next_id += 1;
            id
        });
        *item = idx;
    }
    let new_n = next_id;
    if new_n == n {
        return None; // every community is a singleton: no coarsening
    }

    let mut new_self_weight = vec![0.0; new_n];
    let mut new_adj_map: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); new_n];
    for i in 0..n {
        let ci = renumbered[i];
        new_self_weight[ci] += self_weight[i];
        for &(j, w) in &adj[i] {
            if j < i {
                continue; // adjacency is symmetric; process each edge once
            }
            let cj = renumbered[j];
            if ci == cj {
                new_self_weight[ci] += w;
            } else {
                *new_adj_map[ci].entry(cj).or_insert(0.0) += w;
                *new_adj_map[cj].entry(ci).or_insert(0.0) += w;
            }
        }
    }

    let new_adj: WeightedAdj = new_adj_map
        .into_iter()
        .map(|map| map.into_iter().collect())
        .collect();

    Some(AggregatedLevel {
        renumbered,
        n: new_n,
        adj: new_adj,
        self_weight: new_self_weight,
    })
}

/// Run Louvain community detection on the unified topology (undirected).
///
/// Sums both edge directions into a single undirected weight, ignores
/// self-loops, and restricts membership to `config.node_label` when set
/// (edges touching a node outside the label set are ignored entirely).
/// `cohesion` and `internal_weight` are computed from the original filtered
/// edges, not the internal aggregated levels.
pub(crate) fn louvain(
    topo: &TopologyView,
    idmap: &IdMap,
    syms: &Interner,
    labels: &[u32],
    edge_props: &EdgePropsView,
    config: &LouvainConfig,
) -> CommunityReport {
    let deadline = if config.budget_ms > 0 {
        Some(Instant::now() + Duration::from_millis(config.budget_ms))
    } else {
        None
    };

    // Node set (optionally label-restricted), sorted by key ascending so
    // compact index 0 is always the smallest key — local moving then
    // processes nodes in sorted key order at level 0, and that order
    // propagates deterministically into every aggregated level.
    let (raw_ids, raw_keys) =
        live_nodes_for_label(idmap, syms, labels, config.node_label.as_deref());
    let mut order: Vec<usize> = (0..raw_ids.len()).collect();
    order.sort_by(|&a, &b| raw_keys[a].cmp(&raw_keys[b]));
    let node_ids: Vec<u32> = order.iter().map(|&i| raw_ids[i]).collect();
    let node_keys: Vec<String> = order.iter().map(|&i| raw_keys[i].clone()).collect();
    let n0 = node_ids.len();

    if n0 == 0 {
        return CommunityReport {
            communities: Vec::new(),
            modularity: 0.0,
            truncated: false,
        };
    }

    let mut id_to_idx: BTreeMap<u32, usize> = BTreeMap::new();
    for (i, &id) in node_ids.iter().enumerate() {
        id_to_idx.insert(id, i);
    }

    let etypes = resolve_etypes_multi(syms, topo, &config.edge_types);

    // Collect the original (filtered) undirected weighted edges: sum both
    // directions into one entry per unordered compact-index pair, ignore
    // self-loops, ignore edges touching a node outside the label set.
    // Keyed by (a, b) with a < b so iteration order (and therefore floating
    // point summation order) is deterministic.
    let mut edge_weight_map: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    for &et in &etypes {
        for (i, &id) in node_ids.iter().enumerate() {
            for &nbr in topo.neighbors(et, Direction::Out, id).as_ref() {
                if nbr == id {
                    continue; // ignore self-loops
                }
                let Some(&j) = id_to_idx.get(&nbr) else {
                    continue; // touches a node outside the label restriction
                };
                let Some(w) = edge_weight(
                    edge_props,
                    et,
                    id,
                    nbr,
                    config.weight_prop.as_deref(),
                    config.min_weight,
                ) else {
                    continue; // filtered by min_weight
                };
                let key = if i < j { (i, j) } else { (j, i) };
                *edge_weight_map.entry(key).or_insert(0.0) += w;
            }
        }
    }

    let m: f64 = edge_weight_map.values().sum();

    let mut adj: WeightedAdj = vec![Vec::new(); n0];
    for (&(a, b), &w) in &edge_weight_map {
        adj[a].push((b, w));
        adj[b].push((a, w));
    }
    let mut self_weight: Vec<f64> = vec![0.0; n0];

    // owner[i] = original node i's community index at the current level.
    let mut owner: Vec<usize> = (0..n0).collect();
    let mut truncated = false;
    let mut n = n0;

    if m > 0.0 {
        'passes: for _pass in 0..config.max_passes {
            let (community_of, hit_budget) = local_moving(
                n,
                &adj,
                &self_weight,
                config.resolution,
                config.max_sweeps,
                deadline,
            );
            if hit_budget {
                // Fold this (possibly partial) sweep's assignment straight
                // into owner and stop — no further aggregation, so no
                // renumbering is needed.
                owner = owner.iter().map(|&o| community_of[o]).collect();
                truncated = true;
                break 'passes;
            }
            let Some(level) = aggregate(n, &adj, &self_weight, &community_of) else {
                // Local optimum: no further coarsening. community_of is
                // already the final assignment at this level.
                owner = owner.iter().map(|&o| community_of[o]).collect();
                break 'passes;
            };
            // Compose owner directly through the new level's compact
            // numbering (folds community_of + remap in one step).
            owner = owner.iter().map(|&o| level.renumbered[o]).collect();
            n = level.n;
            adj = level.adj;
            self_weight = level.self_weight;
        }
    }

    // Cohesion / modularity from the ORIGINAL filtered edges, grouped by
    // final community.
    let mut internal: BTreeMap<usize, f64> = BTreeMap::new();
    let mut leaving: BTreeMap<usize, f64> = BTreeMap::new();
    for (&(a, b), &w) in &edge_weight_map {
        let ca = owner[a];
        let cb = owner[b];
        if ca == cb {
            *internal.entry(ca).or_insert(0.0) += w;
        } else {
            *leaving.entry(ca).or_insert(0.0) += w;
            *leaving.entry(cb).or_insert(0.0) += w;
        }
    }

    let mut members_by_community: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (i, key) in node_keys.iter().enumerate() {
        members_by_community
            .entry(owner[i])
            .or_default()
            .push(key.clone());
    }

    let modularity = if m > 0.0 {
        members_by_community
            .keys()
            .map(|c| {
                let internal_w = internal.get(c).copied().unwrap_or(0.0);
                let leaving_w = leaving.get(c).copied().unwrap_or(0.0);
                let sigma_tot = 2.0 * internal_w + leaving_w;
                internal_w / m - config.resolution * (sigma_tot * sigma_tot) / (4.0 * m * m)
            })
            .sum()
    } else {
        0.0
    };

    let mut communities: Vec<Community> = members_by_community
        .into_iter()
        .map(|(c, mut members)| {
            members.sort();
            let internal_w = internal.get(&c).copied().unwrap_or(0.0);
            let leaving_w = leaving.get(&c).copied().unwrap_or(0.0);
            let cohesion = if internal_w + leaving_w > 0.0 {
                internal_w / (internal_w + leaving_w)
            } else {
                1.0
            };
            Community {
                id: 0, // assigned below, after sorting
                members,
                internal_weight: internal_w,
                cohesion,
            }
        })
        .collect();

    communities.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.members[0].cmp(&b.members[0]))
    });
    for (i, c) in communities.iter_mut().enumerate() {
        c.id = i as u32;
    }

    CommunityReport {
        communities,
        modularity,
        truncated,
    }
}

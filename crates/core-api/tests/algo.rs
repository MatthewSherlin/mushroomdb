//! Tests for graph algorithms: PageRank, WCC, degree centrality.
//!
//! Hand-verifiable fixtures, edge-case coverage, as-of instances, write-back
//! round-trips, collision errors, determinism pins, and a property-based WCC
//! test with an independent BFS reference.

use core_api::{
    AlgoDir, DegreeConfig, GraphDb, GraphError, PageRankConfig, Predicate, RuleDef, Value,
    WccConfig,
};
use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-algo-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn open(dir: &std::path::Path) -> GraphDb<core_storage::fs::RealFs> {
    GraphDb::open(dir).expect("open")
}

fn insert_node(db: &mut GraphDb<core_storage::fs::RealFs>, label: &str, key: &str) {
    db.insert_node(label, key, vec![]).unwrap();
}

fn insert_edge(db: &mut GraphDb<core_storage::fs::RealFs>, etype: &str, src: &str, dst: &str) {
    db.insert_edge(etype, src, dst).unwrap();
}

// ---------------------------------------------------------------------------
// PageRank
// ---------------------------------------------------------------------------

/// Star graph: hub in center, 4 spokes pointing to hub.
/// spokes → hub (each spoke has 1 out-edge to hub).
/// Hub has 0 out-edges → dangling.
///
/// With damping 0.85 and 100 iters this should converge.
/// Hub must rank strictly above all spokes.
#[test]
fn pagerank_star_hub_ranks_top() {
    let dir = tmp("pr-star");
    let mut db = open(&dir);
    insert_node(&mut db, "N", "hub");
    for i in 1..=4 {
        insert_node(&mut db, "N", &format!("spoke{i}"));
    }
    for i in 1..=4 {
        insert_edge(&mut db, "POINTS", &format!("spoke{i}"), "hub");
    }
    let config = PageRankConfig {
        max_iters: 100,
        ..PageRankConfig::default()
    };
    let report = db.pagerank(&config);
    assert!(report.converged, "star should converge");
    assert_eq!(report.scores.len(), 5);
    let (top_key, top_score) = &report.scores[0];
    assert_eq!(
        top_key, "hub",
        "hub must rank first, got {:?}",
        report.scores
    );
    // All spokes should have same score (symmetry) and be less than hub.
    for (key, score) in &report.scores[1..] {
        assert!(
            key.starts_with("spoke"),
            "non-hub node should be a spoke, got {key}"
        );
        assert!(
            top_score > score,
            "hub score {top_score} must exceed spoke score {score}"
        );
    }
    // Mass conservation: scores must sum to 1.0 (±1e-6).
    let mass: f64 = report.scores.iter().map(|(_, s)| s).sum();
    assert!(
        (mass - 1.0).abs() < 1e-6,
        "PageRank mass must be conserved (sum ≈ 1.0), got {mass}"
    );
    // Hand-computed hub score for this star topology with d=0.85, N=5:
    //   PR(hub) = 0.132 / 0.252 ≈ 0.5238
    assert!(
        (*top_score - 0.524).abs() < 0.01,
        "hub score should be ≈0.524 (±0.01), got {top_score}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Empty graph → empty scores, converged.
#[test]
fn pagerank_empty_graph() {
    let dir = tmp("pr-empty");
    let db = open(&dir);
    let report = db.pagerank(&PageRankConfig::default());
    assert!(report.scores.is_empty());
    assert!(report.converged);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Single node with no edges → score = 1.0, converged.
#[test]
fn pagerank_single_node() {
    let dir = tmp("pr-single");
    let mut db = open(&dir);
    insert_node(&mut db, "N", "solo");
    let report = db.pagerank(&PageRankConfig::default());
    assert_eq!(report.scores.len(), 1);
    assert_eq!(report.scores[0].0, "solo");
    assert!(
        (report.scores[0].1 - 1.0).abs() < 1e-9,
        "single node score should be 1.0, got {}",
        report.scores[0].1
    );
    assert!(report.converged);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Determinism pin: same graph → same order on repeated calls.
#[test]
fn pagerank_determinism() {
    let dir = tmp("pr-det");
    let mut db = open(&dir);
    for key in ["a", "b", "c"] {
        insert_node(&mut db, "N", key);
    }
    insert_edge(&mut db, "E", "a", "b");
    insert_edge(&mut db, "E", "b", "c");
    insert_edge(&mut db, "E", "c", "a");
    let config = PageRankConfig::default();
    let r1 = db.pagerank(&config);
    let r2 = db.pagerank(&config);
    assert_eq!(r1.scores, r2.scores, "pagerank must be deterministic");
    let _ = std::fs::remove_dir_all(&dir);
}

/// PageRank on derived edges (unified topology showcase).
#[test]
fn pagerank_over_derived_edges() {
    let dir = tmp("pr-derived");
    let mut db = open(&dir);
    // Create two nodes with same tag → rule creates an edge between them.
    db.insert_node("T", "x", vec![("tag".into(), Value::Str("same".into()))])
        .unwrap();
    db.insert_node("T", "y", vec![("tag".into(), Value::Str("same".into()))])
        .unwrap();
    db.create_rule(RuleDef {
        name: "link".into(),
        src_label: "T".into(),
        dst_label: "T".into(),
        predicate: Predicate::FieldEqual {
            field: "tag".into(),
        },
        edge_type: "LINKED".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    // After rule fires: x→y and y→x exist as derived edges.
    let config = PageRankConfig {
        max_iters: 100,
        ..PageRankConfig::default()
    };
    let report = db.pagerank(&config);
    // Both nodes have same in-degree so scores should be equal.
    assert_eq!(report.scores.len(), 2);
    let (_, s0) = &report.scores[0];
    let (_, s1) = &report.scores[1];
    assert!(
        (s0 - s1).abs() < 1e-9,
        "symmetric graph: both nodes should have equal PR, got {s0} vs {s1}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `converged: false` is reported honestly when max_iters is 0.
#[test]
fn pagerank_reports_not_converged_when_zero_iters() {
    let dir = tmp("pr-noconv");
    let mut db = open(&dir);
    insert_node(&mut db, "N", "a");
    insert_node(&mut db, "N", "b");
    insert_edge(&mut db, "E", "a", "b");
    let config = PageRankConfig {
        max_iters: 0,
        ..PageRankConfig::default()
    };
    let report = db.pagerank(&config);
    assert!(!report.converged, "0 iterations must report not converged");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sorted desc by score, then key asc on ties.
#[test]
fn pagerank_sort_order() {
    let dir = tmp("pr-sort");
    let mut db = open(&dir);
    // Chain: a → b → c (b gets rank from a, c gets rank from b)
    for k in ["a", "b", "c"] {
        insert_node(&mut db, "N", k);
    }
    insert_edge(&mut db, "E", "a", "b");
    insert_edge(&mut db, "E", "b", "c");
    let report = db.pagerank(&PageRankConfig {
        max_iters: 100,
        ..PageRankConfig::default()
    });
    let scores: Vec<f64> = report.scores.iter().map(|(_, s)| *s).collect();
    for w in scores.windows(2) {
        assert!(
            w[0] >= w[1],
            "scores must be non-increasing, got {:?}",
            report.scores
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Dangling-node mass conservation: a graph where some nodes have no out-edges.
/// The sum of all PageRank scores must stay ≈ 1.0 despite dangling mass redistribution.
#[test]
fn pagerank_dangling_node_mass_conservation() {
    let dir = tmp("pr-dangle-mass");
    let mut db = open(&dir);
    // a→b, a→c; b and c are dangling (no out-edges).
    for k in ["a", "b", "c"] {
        insert_node(&mut db, "N", k);
    }
    insert_edge(&mut db, "E", "a", "b");
    insert_edge(&mut db, "E", "a", "c");
    let config = PageRankConfig {
        max_iters: 100,
        ..PageRankConfig::default()
    };
    let report = db.pagerank(&config);
    assert!(report.converged, "simple dangling graph should converge");
    let mass: f64 = report.scores.iter().map(|(_, s)| s).sum();
    assert!(
        (mass - 1.0).abs() < 1e-6,
        "dangling-node PR mass must be conserved (sum ≈ 1.0), got {mass}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Edge-type filter using a DERIVED edge type: rule fires LINKED edges,
/// filter to LINKED, results reflect only the derived topology.
#[test]
fn pagerank_derived_edge_type_filter() {
    let dir = tmp("pr-derived-filter");
    let mut db = open(&dir);
    // Two nodes sharing the same tag: rule fires a LINKED edge between them.
    db.insert_node("T", "x", vec![("tag".into(), Value::Str("same".into()))])
        .unwrap();
    db.insert_node("T", "y", vec![("tag".into(), Value::Str("same".into()))])
        .unwrap();
    // Third node connected to x via a manual MANUAL edge only.
    db.insert_node("T", "z", vec![("tag".into(), Value::Str("other".into()))])
        .unwrap();
    insert_edge(&mut db, "MANUAL", "z", "x");
    db.create_rule(RuleDef {
        name: "link-same".into(),
        src_label: "T".into(),
        dst_label: "T".into(),
        predicate: Predicate::FieldEqual {
            field: "tag".into(),
        },
        edge_type: "LINKED".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    // Filter to only LINKED (derived) edges: z has no LINKED edge, so z should score lower.
    let config = PageRankConfig {
        edge_type: Some("LINKED".into()),
        max_iters: 100,
        ..PageRankConfig::default()
    };
    let report = db.pagerank(&config);
    assert_eq!(report.scores.len(), 3, "all 3 nodes must appear");
    // x and y are mutually linked via LINKED; z has no LINKED edges → dangling.
    // x and y must have equal scores (symmetric LINKED graph).
    let x_score = report.scores.iter().find(|(k, _)| k == "x").unwrap().1;
    let y_score = report.scores.iter().find(|(k, _)| k == "y").unwrap().1;
    assert!(
        (x_score - y_score).abs() < 1e-9,
        "x and y should have equal PR under LINKED filter, got x={x_score} y={y_score}"
    );
    // Mass conservation still holds.
    let mass: f64 = report.scores.iter().map(|(_, s)| s).sum();
    assert!(
        (mass - 1.0).abs() < 1e-6,
        "derived-filter PR mass must be conserved, got {mass}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// WCC
// ---------------------------------------------------------------------------

/// Two isolated clusters: {a, b, c} and {x, y}.
/// WCC should find exactly two components.
#[test]
fn wcc_two_clusters() {
    let dir = tmp("wcc-two");
    let mut db = open(&dir);
    for k in ["a", "b", "c", "x", "y"] {
        insert_node(&mut db, "N", k);
    }
    insert_edge(&mut db, "E", "a", "b");
    insert_edge(&mut db, "E", "b", "c");
    insert_edge(&mut db, "E", "x", "y");
    let report = db.connected_components(&WccConfig::default());
    let comp_ids: BTreeSet<&str> = report.components.iter().map(|(_, c)| c.as_str()).collect();
    assert_eq!(
        comp_ids.len(),
        2,
        "expected 2 components, got {:?}",
        report.components
    );
    // Component ID is the smallest key: "a" for {a,b,c} and "x" for {x,y}.
    assert!(comp_ids.contains("a"), "component a-b-c should have id 'a'");
    assert!(comp_ids.contains("x"), "component x-y should have id 'x'");
    // Every node in {a,b,c} should be in comp "a".
    for k in ["a", "b", "c"] {
        let comp = report
            .components
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, c)| c.as_str());
        assert_eq!(comp, Some("a"), "node {k} should be in component 'a'");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Empty graph → empty report.
#[test]
fn wcc_empty_graph() {
    let dir = tmp("wcc-empty");
    let db = open(&dir);
    let report = db.connected_components(&WccConfig::default());
    assert!(report.components.is_empty());
    assert!(!report.truncated);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Single node with no edges → component_id == key.
#[test]
fn wcc_single_node() {
    let dir = tmp("wcc-single");
    let mut db = open(&dir);
    insert_node(&mut db, "N", "solo");
    let report = db.connected_components(&WccConfig::default());
    assert_eq!(report.components.len(), 1);
    assert_eq!(
        report.components[0],
        ("solo".to_string(), "solo".to_string())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Directed edge treated as undirected: a→b means a and b are connected.
#[test]
fn wcc_directed_treated_as_undirected() {
    let dir = tmp("wcc-dir");
    let mut db = open(&dir);
    for k in ["a", "b", "c"] {
        insert_node(&mut db, "N", k);
    }
    // Only directed edges, no back-edges.
    insert_edge(&mut db, "E", "c", "a"); // c→a: they should be connected
                                         // b is isolated.
    let report = db.connected_components(&WccConfig::default());
    let a_comp = report
        .components
        .iter()
        .find(|(k, _)| k == "a")
        .unwrap()
        .1
        .clone();
    let c_comp = report
        .components
        .iter()
        .find(|(k, _)| k == "c")
        .unwrap()
        .1
        .clone();
    assert_eq!(a_comp, c_comp, "a and c must be in same component");
    let b_comp = report
        .components
        .iter()
        .find(|(k, _)| k == "b")
        .unwrap()
        .1
        .clone();
    assert_ne!(a_comp, b_comp, "b must be isolated");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Determinism: same WCC result on repeated calls.
#[test]
fn wcc_determinism() {
    let dir = tmp("wcc-det");
    let mut db = open(&dir);
    for k in ["p", "q", "r", "s"] {
        insert_node(&mut db, "N", k);
    }
    insert_edge(&mut db, "E", "p", "q");
    insert_edge(&mut db, "E", "r", "s");
    let r1 = db.connected_components(&WccConfig::default());
    let r2 = db.connected_components(&WccConfig::default());
    assert_eq!(r1.components, r2.components, "WCC must be deterministic");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// WCC property test: vs. independent BFS reference
// ---------------------------------------------------------------------------

/// Scratch BFS-based WCC reference — entirely independent of production union-find.
///
/// Given an adjacency list, returns a map from node id to its component root
/// (smallest id in the component).
fn bfs_wcc_reference(n: usize, edges: &[(usize, usize)]) -> Vec<usize> {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in edges {
        adj[a].push(b);
        adj[b].push(a);
    }
    let mut comp = vec![usize::MAX; n];
    for start in 0..n {
        if comp[start] != usize::MAX {
            continue;
        }
        let mut queue = VecDeque::new();
        queue.push_back(start);
        comp[start] = start;
        while let Some(v) = queue.pop_front() {
            for &u in &adj[v] {
                if comp[u] == usize::MAX {
                    comp[u] = start; // root = smallest visited = start for BFS from min
                    queue.push_back(u);
                }
            }
        }
    }
    // Normalise: component ID = smallest node in the component.
    let mut min_in_comp: Vec<usize> = (0..n).collect();
    for (i, &root) in comp.iter().enumerate().take(n) {
        if i < min_in_comp[root] {
            min_in_comp[root] = i;
        }
    }
    // Map each node to the true min of its component.
    let mut result = vec![0usize; n];
    for (i, r) in result.iter_mut().enumerate().take(n) {
        *r = min_in_comp[comp[i]];
    }
    result
}

/// Generate a small random graph and compare production WCC against the BFS reference.
///
/// Uses a simple LCG for determinism without proptest.
#[test]
fn wcc_matches_bfs_reference_on_random_graphs() {
    let mut rng: u64 = 0x4d75_7368_726f_6f6d;
    let lcg = |s: &mut u64| -> u64 {
        *s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *s
    };

    for trial in 0..20u32 {
        let n = (lcg(&mut rng) % 12 + 2) as usize; // 2..=13 nodes
        let max_edges = n * 2;
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for _ in 0..max_edges {
            let a = (lcg(&mut rng) % n as u64) as usize;
            let b = (lcg(&mut rng) % n as u64) as usize;
            if a != b && !edges.contains(&(a, b)) && !edges.contains(&(b, a)) {
                edges.push((a, b));
            }
        }

        // Build the reference.
        let ref_comp = bfs_wcc_reference(n, &edges);
        // Number of distinct components from reference.
        let ref_ncomp = ref_comp.iter().copied().collect::<BTreeSet<usize>>().len();

        // Build mushroomdb graph.
        let dir = tmp(&format!("wcc-prop-{trial}"));
        let mut db = open(&dir);
        let keys: Vec<String> = (0..n).map(|i| format!("n{i:02}")).collect();
        for k in &keys {
            insert_node(&mut db, "N", k);
        }
        for &(a, b) in &edges {
            insert_edge(&mut db, "E", &keys[a], &keys[b]);
        }
        let report = db.connected_components(&WccConfig::default());

        // Production component count must match reference.
        let prod_ncomp = report
            .components
            .iter()
            .map(|(_, c)| c.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        assert_eq!(
            prod_ncomp, ref_ncomp,
            "trial {trial}: n={n} edges={edges:?} ref_ncomp={ref_ncomp} prod_ncomp={prod_ncomp}"
        );

        // Every pair that is in the same reference component must be in the same production component.
        for i in 0..n {
            for j in (i + 1)..n {
                let same_ref = ref_comp[i] == ref_comp[j];
                let prod_i = report
                    .components
                    .iter()
                    .find(|(k, _)| k == &keys[i])
                    .unwrap()
                    .1
                    .as_str();
                let prod_j = report
                    .components
                    .iter()
                    .find(|(k, _)| k == &keys[j])
                    .unwrap()
                    .1
                    .as_str();
                let same_prod = prod_i == prod_j;
                assert_eq!(
                    same_ref, same_prod,
                    "trial {trial}: nodes {i} and {j} ref_same={same_ref} prod_same={same_prod}"
                );
            }
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------------------
// Degree centrality
// ---------------------------------------------------------------------------

/// Directed graph: out-degree, in-degree, both.
#[test]
fn degree_directed_vs_undirected() {
    let dir = tmp("deg-dir");
    let mut db = open(&dir);
    // a → b, a → c (a has out-degree 2, b and c have in-degree 1 each)
    for k in ["a", "b", "c"] {
        insert_node(&mut db, "N", k);
    }
    insert_edge(&mut db, "E", "a", "b");
    insert_edge(&mut db, "E", "a", "c");

    // Out-degree: a=2, b=0, c=0.
    let out_cfg = DegreeConfig {
        direction: AlgoDir::Out,
        ..DegreeConfig::default()
    };
    let out_rep = db.degree_centrality(&out_cfg);
    let a_out = out_rep.scores.iter().find(|(k, _)| k == "a").unwrap().1;
    let b_out = out_rep.scores.iter().find(|(k, _)| k == "b").unwrap().1;
    assert_eq!(a_out, 2, "a out-degree should be 2");
    assert_eq!(b_out, 0, "b out-degree should be 0");

    // In-degree: a=0, b=1, c=1.
    let in_cfg = DegreeConfig {
        direction: AlgoDir::In,
        ..DegreeConfig::default()
    };
    let in_rep = db.degree_centrality(&in_cfg);
    let a_in = in_rep.scores.iter().find(|(k, _)| k == "a").unwrap().1;
    let b_in = in_rep.scores.iter().find(|(k, _)| k == "b").unwrap().1;
    let c_in = in_rep.scores.iter().find(|(k, _)| k == "c").unwrap().1;
    assert_eq!(a_in, 0, "a in-degree should be 0");
    assert_eq!(b_in, 1, "b in-degree should be 1");
    assert_eq!(c_in, 1, "c in-degree should be 1");

    // Both: a=2, b=1, c=1.
    let both_cfg = DegreeConfig {
        direction: AlgoDir::Both,
        ..DegreeConfig::default()
    };
    let both_rep = db.degree_centrality(&both_cfg);
    let a_both = both_rep.scores.iter().find(|(k, _)| k == "a").unwrap().1;
    assert_eq!(a_both, 2, "a both-degree should be 2 (out=2, in=0)");
    // Sorted: a first (degree 2), then b and c tied at degree 1.
    assert_eq!(both_rep.scores[0].0, "a");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Empty graph.
#[test]
fn degree_empty_graph() {
    let dir = tmp("deg-empty");
    let db = open(&dir);
    let report = db.degree_centrality(&DegreeConfig::default());
    assert!(report.scores.is_empty());
    assert!(!report.truncated);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Single node, no edges → degree 0.
#[test]
fn degree_single_node() {
    let dir = tmp("deg-single");
    let mut db = open(&dir);
    insert_node(&mut db, "N", "solo");
    let report = db.degree_centrality(&DegreeConfig::default());
    assert_eq!(report.scores.len(), 1);
    assert_eq!(report.scores[0], ("solo".to_string(), 0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sorted desc by degree, key asc on ties.
#[test]
fn degree_sort_order() {
    let dir = tmp("deg-sort");
    let mut db = open(&dir);
    // hub has 3 edges, others have 1.
    for k in ["hub", "s1", "s2", "s3"] {
        insert_node(&mut db, "N", k);
    }
    insert_edge(&mut db, "E", "hub", "s1");
    insert_edge(&mut db, "E", "hub", "s2");
    insert_edge(&mut db, "E", "hub", "s3");
    let report = db.degree_centrality(&DegreeConfig {
        direction: AlgoDir::Out,
        ..DegreeConfig::default()
    });
    assert_eq!(
        report.scores[0].0, "hub",
        "hub should rank first by out-degree"
    );
    // Remaining nodes tied at 0; sorted by key asc.
    let tails: Vec<&str> = report.scores[1..].iter().map(|(k, _)| k.as_str()).collect();
    let mut sorted = tails.clone();
    sorted.sort();
    assert_eq!(tails, sorted, "tied nodes must be sorted by key asc");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// As-of instances — algorithms must work on read-only views
// ---------------------------------------------------------------------------

#[test]
fn algos_work_on_asof_instance() {
    let dir = tmp("algo-asof");
    {
        let mut db = open(&dir);
        insert_node(&mut db, "N", "a");
        insert_node(&mut db, "N", "b");
        insert_edge(&mut db, "E", "a", "b"); // commit 2 (0-based: 2)
        insert_node(&mut db, "N", "c"); // commit 3
    }
    // Open at commit 2 (after edge a→b is present, before c).
    let asof = GraphDb::open_at(&dir, 2).expect("open_at");
    assert!(asof.is_read_only());

    let pr = asof.pagerank(&PageRankConfig::default());
    assert_eq!(pr.scores.len(), 2, "as-of should see 2 nodes, not 3");

    let wcc = asof.connected_components(&WccConfig::default());
    assert_eq!(wcc.components.len(), 2);

    let deg = asof.degree_centrality(&DegreeConfig::default());
    assert_eq!(deg.scores.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// write_scores
// ---------------------------------------------------------------------------

#[test]
fn write_scores_roundtrip() {
    let dir = tmp("ws-roundtrip");
    let mut db = open(&dir);
    insert_node(&mut db, "N", "a");
    insert_node(&mut db, "N", "b");
    let scores = vec![("a".to_string(), 0.9), ("b".to_string(), 0.5)];
    db.write_scores("rank", &scores).expect("write_scores ok");
    // Read back and verify.
    let a_val = db.get_prop("a", "rank");
    let b_val = db.get_prop("b", "rank");
    assert_eq!(a_val, Some(Value::Float(0.9)));
    assert_eq!(b_val, Some(Value::Float(0.5)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_scores_refuses_view_managed_prop() {
    use core_api::{ViewDef, ViewSource};
    let dir = tmp("ws-collision");
    let mut db = open(&dir);
    insert_node(&mut db, "N", "x");
    // Create a Degree view on property "my_degree".
    db.create_view(ViewDef {
        name: "deg_view".into(),
        label: "N".into(),
        view_prop: "my_degree".into(),
        source: ViewSource::Degree {
            edge_type: "E".into(),
            direction: core_api::Direction::Out,
        },
    })
    .expect("create_view");
    // write_scores on the view-managed prop must be refused.
    let result = db.write_scores("my_degree", &[("x".to_string(), 1.0)]);
    match result {
        Err(GraphError::RuleInvalid { detail }) => {
            assert!(
                detail.contains("my_degree"),
                "error should name the prop, got: {detail}"
            );
        }
        other => panic!("expected RuleInvalid, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_scores_refuses_view_name_collision() {
    use core_api::{ViewDef, ViewSource};
    let dir = tmp("ws-name-collision");
    let mut db = open(&dir);
    insert_node(&mut db, "N", "x");
    // Create a view named "my_view" with view_prop "some_prop".
    db.create_view(ViewDef {
        name: "my_view".into(),
        label: "N".into(),
        view_prop: "some_prop".into(),
        source: ViewSource::Degree {
            edge_type: "E".into(),
            direction: core_api::Direction::Out,
        },
    })
    .expect("create_view");
    // Using the view name as prop_name for write_scores must be refused.
    let result = db.write_scores("my_view", &[("x".to_string(), 1.0)]);
    match result {
        Err(GraphError::RuleInvalid { detail }) => {
            assert!(
                detail.contains("my_view"),
                "error should name the colliding view, got: {detail}"
            );
        }
        other => panic!("expected RuleInvalid for name collision, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_scores_refuses_on_readonly() {
    let dir = tmp("ws-readonly");
    {
        let mut db = open(&dir);
        insert_node(&mut db, "N", "a");
    }
    let mut asof = GraphDb::open_at(&dir, 0).expect("open_at");
    let result = asof.write_scores("rank", &[]);
    assert!(
        matches!(result, Err(GraphError::ReadOnly)),
        "write_scores on as-of must return ReadOnly, got {result:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_scores_returns_key_not_found_for_missing_node() {
    let dir = tmp("ws-missing");
    let mut db = open(&dir);
    insert_node(&mut db, "N", "exists");
    let scores = vec![("does-not-exist".to_string(), 0.5)];
    let result = db.write_scores("rank", &scores);
    assert!(
        matches!(result, Err(GraphError::KeyNotFound { .. })),
        "write_scores with unknown key must return KeyNotFound, got {result:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Edge-type filter
// ---------------------------------------------------------------------------

#[test]
fn pagerank_edge_type_filter() {
    let dir = tmp("pr-filter");
    let mut db = open(&dir);
    // Two stars: A→hub via "LIKE", B→hub via "FOLLOW".
    for k in ["hub", "a", "b"] {
        insert_node(&mut db, "N", k);
    }
    insert_edge(&mut db, "LIKE", "a", "hub");
    insert_edge(&mut db, "FOLLOW", "b", "hub");

    // Filter to only LIKE edges: only a→hub present.
    let config = PageRankConfig {
        edge_type: Some("LIKE".into()),
        max_iters: 100,
        ..PageRankConfig::default()
    };
    let report = db.pagerank(&config);
    let hub_score = report.scores.iter().find(|(k, _)| k == "hub").unwrap().1;
    let a_score = report.scores.iter().find(|(k, _)| k == "a").unwrap().1;
    let b_score = report.scores.iter().find(|(k, _)| k == "b").unwrap().1;
    assert!(
        hub_score > a_score,
        "hub must rank above a with LIKE filter"
    );
    // b has no LIKE edges: b is dangling, all scores equal when only 1 edge.
    assert!(
        hub_score > b_score,
        "hub must rank above b with LIKE filter"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wcc_edge_type_filter_nonexistent() {
    // Filter to a type that doesn't exist → all nodes isolated.
    let dir = tmp("wcc-filter");
    let mut db = open(&dir);
    for k in ["a", "b"] {
        insert_node(&mut db, "N", k);
    }
    insert_edge(&mut db, "E", "a", "b");
    let report = db.connected_components(&WccConfig {
        edge_type: Some("NONEXISTENT".into()),
        budget_ms: 5000,
    });
    let comp_ids: BTreeSet<&str> = report.components.iter().map(|(_, c)| c.as_str()).collect();
    assert_eq!(
        comp_ids.len(),
        2,
        "nonexistent etype → 2 isolated components"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Integration: PageRank on demo graph (derived + manual edges)
// ---------------------------------------------------------------------------

#[test]
fn pagerank_on_demo_graph() {
    // Build the full demo (matching cli::run_demo schema) to exercise PageRank
    // on a realistic mixed manual+derived topology.
    use core_api::IngestOptions;
    let dir = tmp("pr-demo");
    let mut db = GraphDb::open(&dir).unwrap();
    let opts = IngestOptions::default();

    // Small slice: 3 orgs, 3 people sharing skills → rules fire.
    let orgs = r#"[
      {"id":"org-01","skills":["rust","graph"]},
      {"id":"org-02","skills":["rust","ml"]},
      {"id":"org-03","skills":["graph","ml"]}
    ]"#;
    let people = r#"[
      {"id":"p-01","skills":["rust","graph"]},
      {"id":"p-02","skills":["rust","ml"]},
      {"id":"p-03","skills":["graph","ml"]}
    ]"#;
    db.ingest_json("Org", orgs, &opts).unwrap();
    db.ingest_json("Person", people, &opts).unwrap();
    db.create_rule(RuleDef {
        name: "overlap".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::Overlap {
            field: "skills".into(),
            min: 0.5,
        },
        edge_type: "FIT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();

    let report = db.pagerank(&PageRankConfig {
        max_iters: 100,
        ..PageRankConfig::default()
    });
    // Some nodes must have positive PR (non-trivial graph).
    assert!(!report.scores.is_empty());
    let max_score = report.scores[0].1;
    assert!(max_score > 0.0, "max PR score must be positive");
    let _ = std::fs::remove_dir_all(&dir);
}

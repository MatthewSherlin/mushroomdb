//! Ledger row 102: a key-anchored one-hop traversal must give the same rows as
//! `node_edges`, however the store was built or read.
//!
//! The planner rewrites `ScanLabel -> Expand -> Filter(key(p) = v)` into a
//! point lookup; these tests pin the *results* (the plan shape is asserted by
//! unit tests in `mushroomdb-query`). Probes: WAL-only vs snapshotted-and-
//! reopened, one batch vs many, residual predicates, param vs literal, the
//! inline `{id: $k}` form, scoped masks, namespaces and `open_at`.

use mushroomdb::{GraphDb, IngestOptions, NodeMask, ResultSet, Value, NS_PROP};
use std::collections::BTreeMap;

const NODES: usize = 3_000;
const FANOUT: usize = 3;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-key-trav-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn person_rows(range: std::ops::Range<usize>) -> Vec<BTreeMap<String, Value>> {
    range
        .map(|i| {
            BTreeMap::from([
                ("id".to_string(), Value::Str(format!("k{i}"))),
                ("score".to_string(), Value::Int((i % 10) as i64)),
            ])
        })
        .collect()
}

fn knows_edges(range: std::ops::Range<usize>) -> Vec<(String, String, String)> {
    range
        .flat_map(|i| {
            (1..=FANOUT).map(move |d| {
                (
                    "KNOWS".to_string(),
                    format!("k{i}"),
                    format!("k{}", (i + d) % NODES),
                )
            })
        })
        .collect()
}

/// All nodes first, then all edges, in one commit each.
fn build_one_batch(dir: &std::path::Path) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    db.ingest("Person", person_rows(0..NODES), &IngestOptions::default())
        .unwrap();
    db.ingest_with_edges(
        "Person",
        vec![],
        &IngestOptions::default(),
        &knows_edges(0..NODES),
    )
    .unwrap();
    db
}

/// Same graph, but written as many small commits.
fn build_many_batches(dir: &std::path::Path) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    for start in (0..NODES).step_by(750) {
        db.ingest(
            "Person",
            person_rows(start..start + 750),
            &IngestOptions::default(),
        )
        .unwrap();
    }
    for start in (0..NODES).step_by(500) {
        db.ingest_with_edges(
            "Person",
            vec![],
            &IngestOptions::default(),
            &knows_edges(start..start + 500),
        )
        .unwrap();
    }
    db
}

fn param(key: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([("k".to_string(), Value::Str(key.to_string()))])
}

fn first_column(rs: &ResultSet) -> Vec<String> {
    let mut out: Vec<String> = (0..rs.len())
        .map(|i| match &rs.row(i)[0] {
            Some(Value::Str(s)) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

fn expected_neighbours(db: &GraphDb<core_storage::fs::RealFs>, key: &str) -> Vec<String> {
    let mut out: Vec<String> = db
        .node_edges(key)
        .unwrap()
        .into_iter()
        .filter(|e| e.src_key == key && e.edge_type == "KNOWS")
        .map(|e| e.dst_key)
        .collect();
    out.sort();
    out
}

const BY_KEY: &str = "MATCH (p:Person)-[:KNOWS]->(q) WHERE key(p) = $k RETURN key(q)";

fn ask(db: &GraphDb<core_storage::fs::RealFs>, cypher: &str, key: &str) -> Vec<String> {
    first_column(
        &db.query(cypher, &param(key))
            .unwrap_or_else(|e| panic!("{cypher}: {e}")),
    )
}

#[test]
fn a_key_anchored_hop_equals_node_edges_before_and_after_a_snapshot_reopen() {
    let dir = tmp("wal-vs-snap");
    let mut db = build_one_batch(&dir);
    let probes = ["k0", "k1", "k1499", "k2999"];
    let wal_only: Vec<Vec<String>> = probes.iter().map(|k| ask(&db, BY_KEY, k)).collect();
    for (k, got) in probes.iter().zip(&wal_only) {
        assert_eq!(got, &expected_neighbours(&db, k), "WAL-only, {k}");
        assert_eq!(got.len(), FANOUT);
    }
    db.snapshot().unwrap();
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    for (k, before) in probes.iter().zip(&wal_only) {
        assert_eq!(&ask(&reopened, BY_KEY, k), before, "reopened, {k}");
    }
}

#[test]
fn one_batch_and_many_commits_answer_a_key_anchored_hop_alike() {
    let one = build_one_batch(&tmp("one"));
    let many = build_many_batches(&tmp("many"));
    for k in ["k0", "k749", "k750", "k2999"] {
        assert_eq!(ask(&one, BY_KEY, k), ask(&many, BY_KEY, k), "{k}");
        assert_eq!(ask(&many, BY_KEY, k), expected_neighbours(&many, k), "{k}");
    }
}

#[test]
fn a_residual_predicate_still_filters_after_the_key_fold() {
    let db = build_one_batch(&tmp("residual"));
    // k10's neighbours are k11, k12, k13 with scores 1, 2, 3.
    let got = ask(
        &db,
        "MATCH (p:Person)-[:KNOWS]->(q) WHERE key(p) = $k AND q.score > 1 RETURN key(q)",
        "k10",
    );
    assert_eq!(got, vec!["k12", "k13"]);
    // A residual on the anchor itself.
    let got = ask(
        &db,
        "MATCH (p:Person)-[:KNOWS]->(q) WHERE key(p) = $k AND p.score > 5 RETURN key(q)",
        "k10",
    );
    assert!(got.is_empty(), "k10 has score 0: {got:?}");
    let got = ask(
        &db,
        "MATCH (p:Person)-[:KNOWS]->(q) WHERE p.score = 6 AND key(p) = $k RETURN key(q)",
        "k16",
    );
    assert_eq!(got, vec!["k17", "k18", "k19"]);
}

#[test]
fn a_literal_and_a_parameter_key_agree() {
    let db = build_one_batch(&tmp("lit"));
    let by_literal = ask(
        &db,
        "MATCH (p:Person)-[:KNOWS]->(q) WHERE key(p) = 'k42' RETURN key(q)",
        "unused",
    );
    assert_eq!(by_literal, ask(&db, BY_KEY, "k42"));
    assert_eq!(by_literal, expected_neighbours(&db, "k42"));
}

#[test]
fn the_inline_key_forms_agree_with_the_where_form() {
    let db = build_one_batch(&tmp("inline"));
    let want = expected_neighbours(&db, "k77");
    let by_id = ask(
        &db,
        "MATCH (p:Person {id: $k})-[:KNOWS]->(q) RETURN key(q)",
        "k77",
    );
    assert_eq!(by_id, want);
    // `{key: $k}` is the stored-wins identity form (a node with no stored
    // `key` is found by its id-map key), so it answers like the others.
    let by_key_prop = ask(
        &db,
        "MATCH (p:Person {key: $k})-[:KNOWS]->(q) RETURN key(q)",
        "k77",
    );
    assert_eq!(by_key_prop, want);
}

#[test]
fn a_missing_wrong_label_or_wrong_type_key_returns_nothing() {
    let db = build_one_batch(&tmp("miss"));
    assert!(ask(&db, BY_KEY, "nobody").is_empty());
    assert!(ask(
        &db,
        "MATCH (p:Robot)-[:KNOWS]->(q) WHERE key(p) = $k RETURN key(q)",
        "k1"
    )
    .is_empty());
    assert!(ask(
        &db,
        "MATCH (p:Person)-[:LIKES]->(q) WHERE key(p) = $k RETURN key(q)",
        "k1"
    )
    .is_empty());
    let none = db
        .query(BY_KEY, &BTreeMap::from([("k".to_string(), Value::Int(1))]))
        .unwrap();
    assert_eq!(none.len(), 0);
}

#[test]
fn a_stored_key_property_does_not_stand_in_for_the_node_key() {
    let mut db = GraphDb::open(&tmp("stored")).unwrap();
    db.insert_node("Person", "real", vec![]).unwrap();
    db.insert_node(
        "Person",
        "impostor",
        vec![("key".into(), Value::Str("real".into()))],
    )
    .unwrap();
    db.insert_node("Person", "friend", vec![]).unwrap();
    db.insert_edge("KNOWS", "real", "friend").unwrap();
    db.insert_edge("KNOWS", "impostor", "real").unwrap();
    assert_eq!(ask(&db, BY_KEY, "real"), vec!["friend"]);
    assert_eq!(ask(&db, BY_KEY, "impostor"), vec!["real"]);
}

#[test]
fn a_mask_hides_the_anchor_and_the_destinations_of_a_key_anchored_hop() {
    let db = build_one_batch(&tmp("mask"));
    let params = param("k5");
    // Anchor and one destination visible; the others hidden.
    let mask = NodeMask::from_keys(&db, ["k5", "k6", "k7000"]);
    let rs = db.query_masked(BY_KEY, &params, &mask).unwrap();
    assert_eq!(first_column(&rs), vec!["k6"]);
    // Anchor hidden: nothing, even though its neighbours are visible.
    let mask = NodeMask::from_keys(&db, ["k6", "k7", "k8"]);
    let rs = db.query_masked(BY_KEY, &params, &mask).unwrap();
    assert_eq!(rs.len(), 0);
    // Full mask equals the unmasked answer.
    let all: Vec<String> = (0..NODES).map(|i| format!("k{i}")).collect();
    let mask = NodeMask::from_keys(&db, all.iter().map(String::as_str));
    let rs = db.query_masked(BY_KEY, &params, &mask).unwrap();
    assert_eq!(first_column(&rs), expected_neighbours(&db, "k5"));
}

#[test]
fn a_namespace_confines_a_key_anchored_hop() {
    let mut db = GraphDb::open(&tmp("ns")).unwrap();
    let in_ns = |ns: &str| vec![(NS_PROP.to_string(), Value::Str(ns.to_string()))];
    db.insert_node("Person", "a", in_ns("tenant-a")).unwrap();
    db.insert_node("Person", "b", in_ns("tenant-a")).unwrap();
    db.insert_node("Person", "x", in_ns("tenant-b")).unwrap();
    db.insert_node("Person", "y", in_ns("tenant-b")).unwrap();
    db.insert_edge("KNOWS", "a", "b").unwrap();
    db.insert_edge("KNOWS", "x", "y").unwrap();
    let mask_a = db.mask_for_namespace("tenant-a");
    let mask_b = db.mask_for_namespace("tenant-b");
    let run = |mask: &NodeMask, key: &str| {
        first_column(&db.query_masked(BY_KEY, &param(key), mask).unwrap())
    };
    assert_eq!(run(&mask_a, "a"), vec!["b"]);
    assert_eq!(run(&mask_b, "x"), vec!["y"]);
    assert!(
        run(&mask_b, "a").is_empty(),
        "the anchor is outside tenant-b"
    );
    assert!(
        run(&mask_a, "x").is_empty(),
        "the anchor is outside tenant-a"
    );
}

#[test]
fn open_at_and_query_at_see_the_graph_as_it_was() {
    let dir = tmp("asof");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "a", vec![]).unwrap();
    db.insert_node("Person", "b", vec![]).unwrap();
    db.insert_node("Person", "c", vec![]).unwrap();
    db.insert_edge("KNOWS", "a", "b").unwrap();
    let after_first_edge = mushroomdb::wal_commit_count_at(&dir).unwrap() - 1;
    db.insert_edge("KNOWS", "a", "c").unwrap();
    let latest = mushroomdb::wal_commit_count_at(&dir).unwrap() - 1;

    let params = param("a");
    let then = db.query_at(after_first_edge, BY_KEY, &params).unwrap();
    assert_eq!(first_column(&then), vec!["b"]);
    let now = db.query_at(latest, BY_KEY, &params).unwrap();
    assert_eq!(first_column(&now), vec!["b", "c"]);

    let temporal = GraphDb::open_at(&dir, after_first_edge).unwrap();
    assert_eq!(ask(&temporal, BY_KEY, "a"), vec!["b"]);
}

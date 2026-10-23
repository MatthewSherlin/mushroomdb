//! `MERGE (a:L {id: 'x'})-[:T]->(b:M {id: 'y'})` (QL-08): find-or-create both
//! nodes and the edge between them, in one batch. The pinned failure mode is a
//! duplicate edge when both nodes and the edge already exist.

use core_storage::wal::{decode_all, WalRecord};
use mushroomdb::{
    schema::Schema, BatchOp, GraphError, Predicate, RealFs, RoleDef, RuleDef, SharedDb, Value,
    WriteScope,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

const MERGE: &str = "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'})";

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-cypher-merge-edge-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn run(db: &mut GraphDb, q: &str) -> mushroomdb::ResultSet {
    db.query_write(q, &p()).unwrap()
}

fn err_of(db: &mut GraphDb, q: &str) -> GraphError {
    db.query_write(q, &p()).unwrap_err()
}

fn detail(e: GraphError) -> String {
    match e {
        GraphError::QueryError { detail } => detail,
        other => panic!("expected QueryError, got {other:?}"),
    }
}

/// `(type, src, dst)` of every edge touching `key`, sorted.
fn edges(db: &GraphDb, key: &str) -> Vec<(String, String, String)> {
    let mut v: Vec<_> = db
        .node_edges(key)
        .unwrap()
        .into_iter()
        .map(|e| (e.edge_type, e.src_key, e.dst_key))
        .collect();
    v.sort();
    v.dedup();
    v
}

fn k(src: &str, dst: &str) -> (String, String, String) {
    ("K".into(), src.into(), dst.into())
}

/// Counts of `InsertNode` / `InsertEdge` / `SetProp` records in the WAL.
fn wal_counts(dir: &std::path::Path) -> (usize, usize, usize) {
    let wal = std::fs::read(dir.join("wal.bin")).unwrap();
    let (records, _) = decode_all(&wal);
    let (mut nodes, mut edges, mut sets) = (0, 0, 0);
    for r in records.iter().flat_map(|r| match r {
        WalRecord::Batch(inner) => inner.iter().collect::<Vec<_>>(),
        other => vec![other],
    }) {
        match r {
            WalRecord::InsertNode { .. } | WalRecord::InsertNodeId { .. } => nodes += 1,
            WalRecord::InsertEdge { .. } | WalRecord::InsertEdgeId { .. } => edges += 1,
            WalRecord::SetProp { .. } | WalRecord::SetPropId { .. } => sets += 1,
            _ => {}
        }
    }
    (nodes, edges, sets)
}

fn same_region_rule() -> RuleDef {
    RuleDef {
        name: "same-region".into(),
        src_label: "P".into(),
        dst_label: "P".into(),
        predicate: Predicate::FieldEqual {
            field: "region".into(),
        },
        edge_type: "SAME".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

// ── Create and idempotence ──────────────────────────────────────────────────

#[test]
fn creates_both_nodes_and_the_edge_when_none_exist() {
    let mut db = GraphDb::open(&tmp("create-all")).unwrap();
    let rs = run(&mut db, MERGE);
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(2)));
    assert_eq!(rs.get(0, "relationships_created"), Some(&Value::Int(1)));
    assert!(db.has_node("a") && db.has_node("b"));
    assert_eq!(edges(&db, "a"), vec![k("a", "b")]);
    assert_eq!(db.get_prop("a", "id"), Some(Value::Str("a".into())));
}

#[test]
fn creates_only_the_edge_between_existing_nodes() {
    let mut db = GraphDb::open(&tmp("create-edge")).unwrap();
    run(&mut db, "CREATE (a:P {id: 'a'})");
    run(&mut db, "CREATE (b:P {id: 'b'})");
    let rs = run(&mut db, MERGE);
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(0)));
    assert_eq!(rs.get(0, "relationships_created"), Some(&Value::Int(1)));
    assert_eq!(edges(&db, "b"), vec![k("a", "b")]);
}

#[test]
fn creates_the_missing_end_when_only_the_start_exists() {
    let mut db = GraphDb::open(&tmp("create-end")).unwrap();
    run(&mut db, "CREATE (a:P {id: 'a', x: 1})");
    let rs = run(&mut db, MERGE);
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(1)));
    assert_eq!(rs.get(0, "relationships_created"), Some(&Value::Int(1)));
    assert_eq!(
        db.get_prop("a", "x"),
        Some(Value::Int(1)),
        "start untouched"
    );
    assert_eq!(edges(&db, "a"), vec![k("a", "b")]);
}

#[test]
fn the_same_merge_twice_does_not_duplicate_the_edge() {
    let dir = tmp("idempotent");
    let mut db = GraphDb::open(&dir).unwrap();
    run(&mut db, MERGE);
    let before = db.edge_count();
    let seq = db.commit_seq();
    let rs = run(&mut db, MERGE);
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(0)));
    assert_eq!(db.edge_count(), before);
    assert_eq!(edges(&db, "a"), vec![k("a", "b")]);
    assert_eq!(db.commit_seq(), seq, "a matching MERGE writes nothing");
    assert_eq!(wal_counts(&dir), (2, 1, 0));
}

#[test]
fn both_nodes_and_the_edge_existing_is_a_match_not_a_second_edge() {
    // Pinned (Review Focus 4): the edge was made another way.
    let dir = tmp("pinned");
    let mut db = GraphDb::open(&dir).unwrap();
    run(&mut db, "CREATE (a:P {id: 'a'})");
    run(&mut db, "CREATE (b:P {id: 'b'})");
    db.insert_edge("K", "a", "b").unwrap();
    let before = db.edge_count();
    run(&mut db, MERGE);
    run(&mut db, MERGE);
    assert_eq!(db.edge_count(), before);
    assert_eq!(edges(&db, "a"), vec![k("a", "b")]);
    assert_eq!(wal_counts(&dir).1, 1, "no second InsertEdge was logged");
}

#[test]
fn direction_type_and_endpoints_each_make_a_distinct_edge() {
    let mut db = GraphDb::open(&tmp("distinct")).unwrap();
    run(&mut db, MERGE);
    // The reverse edge is a different edge.
    run(&mut db, "MERGE (b:P {id: 'b'})-[:K]->(a:P {id: 'a'})");
    // A different type between the same pair too.
    run(&mut db, "MERGE (a:P {id: 'a'})-[:J]->(b:P {id: 'b'})");
    // And a different end.
    run(&mut db, "MERGE (a:P {id: 'a'})-[:K]->(c:P {id: 'c'})");
    let got = edges(&db, "a");
    assert!(got.contains(&k("a", "b")) && got.contains(&k("b", "a")));
    assert!(got.contains(&("J".into(), "a".into(), "b".into())));
    assert!(got.contains(&k("a", "c")));
    assert_eq!(got.len(), 4);
}

#[test]
fn a_left_arrow_creates_the_edge_from_the_end_node_to_the_start() {
    let mut db = GraphDb::open(&tmp("left")).unwrap();
    run(&mut db, "MERGE (a:P {id: 'a'})<-[:K]-(b:P {id: 'b'})");
    assert_eq!(edges(&db, "a"), vec![k("b", "a")]);
    let before = db.edge_count();
    run(&mut db, "MERGE (a:P {id: 'a'})<-[:K]-(b:P {id: 'b'})");
    // Written the other way round it is the same edge.
    run(&mut db, "MERGE (b:P {id: 'b'})-[:K]->(a:P {id: 'a'})");
    assert_eq!(db.edge_count(), before);
}

#[test]
fn a_self_loop_creates_one_node_and_one_edge() {
    let mut db = GraphDb::open(&tmp("loop")).unwrap();
    let rs = run(&mut db, "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'a'})");
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(1)));
    assert_eq!(rs.get(0, "relationships_created"), Some(&Value::Int(1)));
    assert_eq!(edges(&db, "a"), vec![k("a", "a")]);
    run(&mut db, "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'a'})");
    assert_eq!(db.edge_count(), 1);
    // One key under two labels is not a node pattern.
    let e = detail(err_of(
        &mut db,
        "MERGE (a:P {id: 'z'})-[:K]->(b:Q {id: 'z'})",
    ));
    assert!(e.contains("both label"), "{e}");
    assert!(!db.has_node("z"));
}

#[test]
fn the_end_node_may_name_a_namespace() {
    let mut db = GraphDb::open(&tmp("end-ns")).unwrap();
    run(
        &mut db,
        "MERGE (a:P {id: 'a', ns: 'tenant'})-[:K]->(b:P {id: 'b', ns: 'tenant'})",
    );
    assert_eq!(db.namespace_of("a").as_deref(), Some("tenant"));
    assert_eq!(db.namespace_of("b").as_deref(), Some("tenant"));
    // An edge across namespaces is refused as everywhere else, and the whole
    // statement is unapplied.
    let e = err_of(
        &mut db,
        "MERGE (c:P {id: 'c'})-[:K]->(d:P {id: 'd', ns: 'tenant'})",
    );
    assert!(matches!(e, GraphError::CrossNamespace { .. }), "{e:?}");
    assert!(!db.has_node("c") && !db.has_node("d"));
}

// ── ON CREATE / ON MATCH ────────────────────────────────────────────────────

#[test]
fn on_create_set_runs_on_the_first_merge_only() {
    let mut db = GraphDb::open(&tmp("on-create")).unwrap();
    run(&mut db, &format!("{MERGE} ON CREATE SET a.new = true"));
    assert_eq!(db.get_prop("a", "new"), Some(Value::Bool(true)));
    run(&mut db, "MATCH (a:P {id: 'a'}) SET a.new = false");
    run(&mut db, &format!("{MERGE} ON CREATE SET a.new = true"));
    assert_eq!(
        db.get_prop("a", "new"),
        Some(Value::Bool(false)),
        "second run is a match: ON CREATE did not run"
    );
}

#[test]
fn on_match_set_runs_on_the_second_merge_only() {
    let mut db = GraphDb::open(&tmp("on-match")).unwrap();
    run(&mut db, &format!("{MERGE} ON MATCH SET a.seen = true"));
    assert_eq!(db.get_prop("a", "seen"), None, "first run created");
    run(&mut db, &format!("{MERGE} ON MATCH SET a.seen = true"));
    assert_eq!(db.get_prop("a", "seen"), Some(Value::Bool(true)));
}

#[test]
fn both_clauses_and_the_end_node_variable_in_set() {
    let mut db = GraphDb::open(&tmp("both")).unwrap();
    let q = format!("{MERGE} ON CREATE SET a.n = 1, b.n = 10 ON MATCH SET a.n = 2, b.n = 20");
    run(&mut db, &q);
    assert_eq!(db.get_prop("a", "n"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("b", "n"), Some(Value::Int(10)));
    run(&mut db, &q);
    assert_eq!(db.get_prop("a", "n"), Some(Value::Int(2)));
    assert_eq!(db.get_prop("b", "n"), Some(Value::Int(20)));
}

#[test]
fn a_new_edge_between_existing_nodes_is_a_create_for_on_create() {
    let mut db = GraphDb::open(&tmp("create-between")).unwrap();
    run(&mut db, "CREATE (a:P {id: 'a'})");
    run(&mut db, "CREATE (b:P {id: 'b'})");
    run(
        &mut db,
        &format!("{MERGE} ON CREATE SET a.c = 1 ON MATCH SET a.m = 1"),
    );
    assert_eq!(db.get_prop("a", "c"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("a", "m"), None);
}

#[test]
fn set_values_may_be_parameters() {
    let mut db = GraphDb::open(&tmp("params")).unwrap();
    let mut params = p();
    params.insert("v".into(), Value::Int(7));
    db.query_write(&format!("{MERGE} ON CREATE SET b.v = $v"), &params)
        .unwrap();
    assert_eq!(db.get_prop("b", "v"), Some(Value::Int(7)));
}

#[test]
fn the_rules_hook_fires_for_on_create_and_on_match_assignments() {
    let mut db = GraphDb::open(&tmp("rules")).unwrap();
    db.create_rule(same_region_rule()).unwrap();
    run(&mut db, "CREATE (c:P {id: 'c', region: 'x'})");
    // ON CREATE puts `a` into region x: the rule derives SAME a<->c.
    run(&mut db, &format!("{MERGE} ON CREATE SET a.region = 'x'"));
    let same = |db: &GraphDb| edges(db, "a").into_iter().filter(|e| e.0 == "SAME").count();
    assert!(same(&db) > 0, "ON CREATE assignment derived an edge");
    // ON MATCH moves `a` out: the rule retracts.
    run(&mut db, &format!("{MERGE} ON MATCH SET a.region = 'y'"));
    assert_eq!(same(&db), 0, "ON MATCH assignment retracted it");
}

// ── RETURN ──────────────────────────────────────────────────────────────────

#[test]
fn return_projects_both_nodes_and_the_relationship() {
    let mut db = GraphDb::open(&tmp("return")).unwrap();
    for _ in 0..2 {
        let rs = run(
            &mut db,
            "MERGE (a:P {id: 'a'})-[r:K]->(b:P {id: 'b'}) \
             ON CREATE SET b.n = 1 RETURN a.id AS a, b.id AS b, b.n AS n, type(r) AS t",
        );
        assert_eq!(rs.len(), 1);
        assert_eq!(rs.get(0, "a"), Some(&Value::Str("a".into())));
        assert_eq!(rs.get(0, "b"), Some(&Value::Str("b".into())));
        assert_eq!(rs.get(0, "n"), Some(&Value::Int(1)));
        assert_eq!(rs.get(0, "t"), Some(&Value::Str("K".into())));
    }
}

// ── Refusals ────────────────────────────────────────────────────────────────

#[test]
fn unsupported_shapes_are_refused_by_name_and_write_nothing() {
    let mut db = GraphDb::open(&tmp("refusals")).unwrap();
    for (q, want) in [
        (
            "MERGE (a:P {id: 'a'})-[:K]-(b:P {id: 'b'})",
            "undirected relationship",
        ),
        (
            "MERGE (a:P {id: 'a'})-[:K*1..2]->(b:P {id: 'b'})",
            "variable-length",
        ),
        (
            "MERGE (a:P {id: 'a'})-[:K|J]->(b:P {id: 'b'})",
            "alternative relationship types",
        ),
        (
            "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'})-[:K]->(c:P {id: 'c'})",
            "more than one relationship",
        ),
        (
            "MERGE (a:P {id: 'a'})-[]->(b:P {id: 'b'})",
            "requires a relationship type",
        ),
    ] {
        let e = db.query_write(q, &p()).unwrap_err().to_string();
        assert!(e.contains(want), "{q}: {e}");
    }
    assert!(!db.has_node("a") && !db.has_node("b"));
}

#[test]
fn set_on_the_relationship_or_an_unbound_variable_is_refused() {
    let mut db = GraphDb::open(&tmp("set-vars")).unwrap();
    for q in [
        "MERGE (a:P {id: 'a'})-[r:K]->(b:P {id: 'b'}) ON CREATE SET r.w = 1",
        "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'}) ON CREATE SET z.w = 1",
        "MERGE (:P {id: 'a'})-[:K]->(b:P {id: 'b'}) ON CREATE SET a.w = 1",
    ] {
        let e = detail(err_of(&mut db, q));
        assert!(
            e.contains("does not match a MERGE node variable"),
            "{q}: {e}"
        );
    }
    assert!(!db.has_node("a"), "statement unapplied");
}

/// QL-PM: a map SET under a relationship MERGE sets the map on that end.
#[test]
fn a_map_set_under_merge_sets_the_map() {
    let mut db = GraphDb::open(&tmp("map-set")).unwrap();
    db.query_write(&format!("{MERGE} ON CREATE SET a += {{x: 1}}"), &p())
        .unwrap();
    assert_eq!(db.get_prop("a", "x"), Some(Value::Int(1)));
}

#[test]
fn the_id_property_rule_applies_and_the_statement_is_unapplied() {
    let mut db = GraphDb::open(&tmp("id-rule")).unwrap();
    let e = db
        .query_write(&format!("{MERGE} ON CREATE SET b.id = 'other'"), &p())
        .unwrap_err()
        .to_string();
    assert!(e.contains("rename_node"), "{e}");
    assert!(!db.has_node("a") && !db.has_node("b"), "nothing created");
    // Restating the key is allowed.
    run(&mut db, &format!("{MERGE} ON CREATE SET b.id = 'b'"));
    assert!(db.has_node("b"));
}

#[test]
fn a_constraint_violation_in_on_create_leaves_no_node_or_edge() {
    let mut db = GraphDb::open(&tmp("constraint")).unwrap();
    let e = db
        .query_write(&format!("{MERGE} ON CREATE SET a.big = [null]"), &p())
        .unwrap_err()
        .to_string();
    assert!(!e.is_empty());
    assert!(!db.has_node("a") && db.edge_count() == 0, "atomic: {e}");
}

// ── Differential probes ─────────────────────────────────────────────────────

fn state(db: &GraphDb) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["a", "b", "c"] {
        if db.has_node(key) {
            out.push(format!(
                "{key} {:?} {:?}",
                db.node_info(key).unwrap().props,
                edges(db, key)
            ));
        }
    }
    out
}

#[test]
fn merge_is_identical_one_statement_or_many_live_replayed_snapshotted_and_at() {
    let script = [
        "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'}) ON CREATE SET a.n = 1 ON MATCH SET a.n = 2",
        "MERGE (a:P {id: 'a'})-[:K]->(b:P {id: 'b'}) ON CREATE SET a.n = 1 ON MATCH SET a.n = 2",
        "MERGE (b:P {id: 'b'})<-[:K]-(c:P {id: 'c'}) ON CREATE SET c.n = 9",
    ];
    // Each statement is one batch; a store that gets the same facts through
    // explicit writes must be indistinguishable.
    let mut manual = GraphDb::open(&tmp("diff-manual")).unwrap();
    manual
        .batch()
        .insert_node("P", "a", vec![("id".into(), Value::Str("a".into()))])
        .insert_node("P", "b", vec![("id".into(), Value::Str("b".into()))])
        .insert_edge("K", "a", "b")
        .set_prop("a", "n", Value::Int(1))
        .commit()
        .unwrap();
    manual
        .batch()
        .set_prop("a", "n", Value::Int(2))
        .commit()
        .unwrap();
    manual
        .batch()
        .insert_node("P", "c", vec![("id".into(), Value::Str("c".into()))])
        .insert_edge("K", "c", "b")
        .set_prop("c", "n", Value::Int(9))
        .commit()
        .unwrap();

    let dir = tmp("diff-merge");
    let mut db = GraphDb::open(&dir).unwrap();
    for q in script {
        run(&mut db, q);
    }
    let live = state(&db);
    assert_eq!(live, state(&manual), "MERGE vs explicit batches");
    assert_eq!(db.get_prop("a", "n"), Some(Value::Int(2)));
    assert_eq!(db.edge_count(), 2);

    let last = db.commit_seq();
    let at = GraphDb::open_at(&dir, last - 1).unwrap();
    assert_eq!(state(&at), live, "open_at");
    drop(at);
    drop(db);
    let mut reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(state(&reopened), live, "WAL replay");
    reopened.snapshot().unwrap();
    drop(reopened);
    let mut after = GraphDb::open(&dir).unwrap();
    assert_eq!(state(&after), live, "after a snapshot");
    // A matching MERGE after the snapshot still finds the edge and adds none.
    run(&mut after, script[0]);
    assert_eq!(after.edge_count(), 2);
    assert_eq!(after.get_prop("a", "n"), Some(Value::Int(2)));
    drop(after);
    let again = GraphDb::open(&dir).unwrap();
    assert_eq!(
        again.edge_count(),
        2,
        "reopened after the post-snapshot MERGE"
    );
}

// ── Inside a transaction ────────────────────────────────────────────────────

#[test]
fn merge_inside_a_transaction_buffers_is_idempotent_and_commits() {
    let dir = tmp("tx");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    let mut tx = db.begin_transaction();
    let rs = tx
        .execute(&format!("{MERGE} ON CREATE SET a.n = 1"), &p())
        .unwrap();
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(2)));
    assert_eq!(rs.get(0, "relationships_created"), Some(&Value::Int(1)));
    // Second run inside the same transaction sees the buffered edge.
    let rs = tx
        .execute(
            &format!("{MERGE} ON CREATE SET a.n = 1 ON MATCH SET a.m = 1"),
            &p(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "created"), Some(&Value::Int(0)));
    let inside = tx
        .execute(
            "MATCH (a:P {id: 'a'})-[:K]->(b:P) RETURN a.n AS n, a.m AS m, b.id AS b",
            &p(),
        )
        .unwrap();
    assert_eq!(inside.len(), 1, "one edge, not two");
    assert_eq!(inside.get(0, "n"), Some(&Value::Int(1)));
    assert_eq!(inside.get(0, "m"), Some(&Value::Int(1)));
    // Not visible outside until commit.
    assert!(db.reader().node_info("a").is_none());
    tx.commit().unwrap();
    let info = db.reader().node_info("a").unwrap();
    assert_eq!(info.props.get("m"), Some(&Value::Int(1)));
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(edges(&reopened, "a"), vec![k("a", "b")]);
}

#[test]
fn merge_inside_a_transaction_finds_an_edge_committed_before_it() {
    let dir = tmp("tx-existing");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![
        BatchOp::InsertNode {
            label: "P".into(),
            key: "a".into(),
            props: vec![],
        },
        BatchOp::InsertNode {
            label: "P".into(),
            key: "b".into(),
            props: vec![],
        },
        BatchOp::InsertEdge {
            edge_type: "K".into(),
            src_key: "a".into(),
            dst_key: "b".into(),
        },
    ])
    .unwrap();
    let mut tx = db.begin_transaction();
    tx.execute(&format!("{MERGE} ON MATCH SET a.hit = 1"), &p())
        .unwrap();
    tx.commit().unwrap();
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.get_prop("a", "hit"), Some(Value::Int(1)));
    assert_eq!(reopened.edge_count(), 1);
}

#[test]
fn a_rolled_back_merge_leaves_nothing() {
    let dir = tmp("tx-rollback");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute(MERGE, &p()).unwrap();
    tx.rollback();
    assert!(db.reader().node_info("a").is_none());
}

#[test]
fn a_refused_merge_inside_a_transaction_buffers_nothing() {
    let db: SharedDb = SharedDb::open(&tmp("tx-refused")).unwrap();
    let mut tx = db.begin_transaction();
    assert!(tx
        .execute(&format!("{MERGE} ON CREATE SET b.id = 'other'"), &p())
        .is_err());
    let rs = tx.execute("MATCH (n:P) RETURN n.id AS id", &p()).unwrap();
    assert_eq!(rs.len(), 0);
    tx.commit().unwrap();
}

/// `a` and `b` committed, with the `K` edge between them when `with_edge`.
fn two_nodes(db: &SharedDb, with_edge: bool) {
    let mut ops = vec![
        BatchOp::InsertNode {
            label: "P".into(),
            key: "a".into(),
            props: vec![("id".into(), Value::Str("a".into()))],
        },
        BatchOp::InsertNode {
            label: "P".into(),
            key: "b".into(),
            props: vec![("id".into(), Value::Str("b".into()))],
        },
    ];
    if with_edge {
        ops.push(BatchOp::InsertEdge {
            edge_type: "K".into(),
            src_key: "a".into(),
            dst_key: "b".into(),
        });
    }
    db.submit_batch(ops).unwrap();
}

/// Both ends of a buffered MERGE are in the transaction's conflict set,
/// whether the MERGE creates the edge or only matches it (ON MATCH writes the
/// start node alone, and the end node is still read).
#[test]
fn a_merge_inside_a_transaction_conflicts_with_a_write_to_either_end() {
    for with_edge in [false, true] {
        for end in ["a", "b"] {
            let name = format!("tx-conflict-{with_edge}-{end}");
            let db: SharedDb = SharedDb::open(&tmp(&name)).unwrap();
            two_nodes(&db, with_edge);
            let mut tx = db.begin_transaction();
            tx.execute(&format!("{MERGE} ON MATCH SET a.hit = 1"), &p())
                .unwrap();
            db.submit_batch(vec![BatchOp::SetProp {
                key: end.into(),
                field: "x".into(),
                value: Value::Int(1),
            }])
            .unwrap();
            let err = tx.commit().unwrap_err();
            assert!(
                matches!(err, GraphError::CasConflict { ref key, .. } if key == end),
                "edge existed: {with_edge}, concurrent write to {end}: {err:?}"
            );
            assert_eq!(
                db.reader().node_info("a").unwrap().props.get("hit"),
                None,
                "a conflicted commit writes nothing"
            );
        }
    }
}

/// A MERGE's RETURN inside a transaction reads the statement's own writes
/// through the overlay; a RETURN that fails takes the statement's writes back
/// out of the overlay and buffers nothing.
#[test]
fn a_merge_return_inside_a_transaction_reads_its_writes_and_a_failed_one_buffers_nothing() {
    let dir = tmp("tx-return");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    let mut tx = db.begin_transaction();
    let rs = tx
        .execute(
            "MERGE (a:P {id: 'a'})-[r:K]->(b:P {id: 'b'}) ON CREATE SET b.n = 7 \
             RETURN a.id AS a, b.n AS n, type(r) AS t",
            &p(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "a"), Some(&Value::Str("a".into())));
    assert_eq!(rs.get(0, "n"), Some(&Value::Int(7)));
    assert_eq!(rs.get(0, "t"), Some(&Value::Str("K".into())));
    assert!(tx
        .execute(
            "MERGE (a:P {id: 'c'})-[:K]->(b:P {id: 'd'}) RETURN no_such_function(a) AS x",
            &p(),
        )
        .is_err());
    let inside = tx
        .execute("MATCH (n:P) RETURN n.id AS id ORDER BY id", &p())
        .unwrap();
    assert_eq!(inside.len(), 2, "c and d were taken back out");
    let edges_inside = tx
        .execute("MATCH (:P)-[r:K]->(:P) RETURN count(r) AS c", &p())
        .unwrap();
    assert_eq!(edges_inside.get(0, "c"), Some(&Value::Int(1)));
    tx.commit().unwrap();
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.node_count(), 2);
    assert_eq!(reopened.edge_count(), 1);
    assert_eq!(reopened.get_prop("b", "n"), Some(Value::Int(7)));
}

// ── Role-scoped writes ──────────────────────────────────────────────────────

fn scoped_store(name: &str, create_labels: &[&str], edge_types: &[&str]) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "writer".into(),
            keys: vec![],
            labels: vec!["Mine".into()],
            visible_where: None,
            namespaces: None,
            write: Some(WriteScope {
                create_labels: create_labels.iter().map(|s| s.to_string()).collect(),
                update_labels: vec!["Mine".into()],
                delete_labels: vec![],
                create_edge_types: edge_types.iter().map(|s| s.to_string()).collect(),
                delete_edge_types: vec![],
            }),
        }],
    })
    .unwrap();
    db.insert_node("Mine", "m1", vec![]).unwrap();
    db.insert_node("Mine", "m2", vec![]).unwrap();
    db.insert_node("Secret", "s", vec![]).unwrap();
    db
}

#[test]
fn a_scoped_merge_between_visible_nodes_creates_the_edge_once() {
    let mut db = scoped_store("scoped-ok", &["Mine"], &["K"]);
    let q = "MERGE (a:Mine {id: 'm1'})-[:K]->(b:Mine {id: 'm2'}) ON CREATE SET a.n = 1";
    db.query_write_authz("writer", q, &p()).unwrap();
    db.query_write_authz("writer", q, &p()).unwrap();
    assert_eq!(db.edge_count(), 1);
    assert_eq!(db.get_prop("m1", "n"), Some(Value::Int(1)));
}

#[test]
fn a_scoped_merge_cannot_reach_a_hidden_node_and_says_the_same_as_for_an_absent_one() {
    let mut db = scoped_store("scoped-hidden", &[], &["K"]);
    let hidden = db
        .query_write_authz(
            "writer",
            "MERGE (a:Mine {id: 'm1'})-[:K]->(b:Mine {id: 's'})",
            &p(),
        )
        .unwrap_err()
        .to_string();
    let absent = db
        .query_write_authz(
            "writer",
            "MERGE (a:Mine {id: 'm1'})-[:K]->(b:Mine {id: 'nobody'})",
            &p(),
        )
        .unwrap_err()
        .to_string();
    assert_eq!(hidden, absent, "hidden is indistinguishable from absent");
    let out_of_scope = db
        .query_write_authz(
            "writer",
            "MERGE (a:Mine {id: 'm1'})-[:K]->(b:Secret {id: 's'})",
            &p(),
        )
        .unwrap_err();
    assert!(matches!(out_of_scope, GraphError::RoleWriteDenied { .. }));
    assert_eq!(db.edge_count(), 0);
}

#[test]
fn a_scoped_merge_needs_the_edge_type_in_scope() {
    let mut db = scoped_store("scoped-etype", &["Mine"], &["J"]);
    let e = db
        .query_write_authz(
            "writer",
            "MERGE (a:Mine {id: 'm1'})-[:K]->(b:Mine {id: 'm2'})",
            &p(),
        )
        .unwrap_err();
    assert!(matches!(e, GraphError::RoleWriteDenied { .. }), "{e:?}");
    assert_eq!(db.edge_count(), 0);
}
